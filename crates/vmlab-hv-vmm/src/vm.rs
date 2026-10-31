//! The assembled machine: shared state, the run-state machine, the exit
//! path, and the handle the control server drives it with.

use std::sync::Arc;

use kvm_ioctls::VcpuFd;
use parking_lot::{Condvar, Mutex};
use vmlab_hv_proto::control::{Event, RunState, ShutdownReason};

use vmlab_hv_devices::usb::{UsbDevice, UsbSpeed};

use crate::bus::{IoBus, MmioBus};
use crate::error::VmmError;
use crate::kvm::KvmVm;
use crate::layout::{IO_ALLOC_BASE, MMIO32_BASE, MMIO32_SIZE};
use crate::memory::GuestRam;
use crate::pci::PciRoot;
use crate::pm::{PmDevice, SleepSignal, SleepState};
use crate::vcpu::VcpuThread;

/// One host USB device the control protocol attached: the id vmlab chose
/// and the root port it landed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbAttachment {
    pub id: String,
    pub port: u8,
    pub hostbus: u16,
    pub hostaddr: u16,
    pub hostport: String,
}

/// Why the machine stopped for good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    Shutdown(ShutdownReason),
    Error(String),
}

/// Broadcasts machine events to the control connections.
pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Default)]
struct MemoryGate {
    blocked: bool,
    in_kvm: usize,
}

pub(crate) struct KvmRunGuard<'a>(&'a VmShared);
impl Drop for KvmRunGuard<'_> {
    fn drop(&mut self) {
        self.0.memory_gate.lock().in_kvm -= 1;
        self.0.memory_cv.notify_all();
    }
}

struct MemoryUpdateGuard<'a> {
    shared: &'a VmShared,
    _writer: parking_lot::MutexGuard<'a, ()>,
}
impl Drop for MemoryUpdateGuard<'_> {
    fn drop(&mut self) {
        self.shared.memory_gate.lock().blocked = false;
        self.shared.memory_cv.notify_all();
    }
}

/// Everything the vCPU threads and the control threads share.
pub struct VmShared {
    pub(crate) vfio: Mutex<Vec<Arc<Mutex<crate::vfio::AssignedDevice>>>>,
    io_trace: Option<Mutex<std::collections::VecDeque<String>>>,
    pub(crate) display: Mutex<Option<Arc<vmlab_hv_display::DisplayServer>>>,
    pub(crate) display_generation: std::sync::atomic::AtomicU64,
    vga_mapping: Mutex<Option<u64>>,
    last_frame: Mutex<std::time::Instant>,
    memory_gate: Mutex<MemoryGate>,
    memory_cv: Condvar,
    memory_writer: Mutex<()>,
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
    pub(crate) mch: Mutex<Option<Arc<Mutex<crate::q35::Mch>>>>,
    pub(crate) chipset: Mutex<Option<Arc<crate::chipset::Chipset>>>,
    pub(crate) vga: Mutex<Option<crate::display::Video>>,
    pub(crate) xhci: Mutex<Option<Arc<Mutex<vmlab_hv_devices::usb::XhciController>>>>,
    /// Whether the machine's description asked for the `usbx` passthrough
    /// controller (`usb = true`): only such machines accept hot-plug.
    pub(crate) usb_hotplug: std::sync::atomic::AtomicBool,
    /// Host USB devices attached through the control protocol.
    pub(crate) usb_attachments: Mutex<Vec<UsbAttachment>>,
    pub(crate) pflash: Mutex<Option<Arc<Mutex<vmlab_hv_devices::pflash::Pflash>>>>,
    pub(crate) tpm: Mutex<Option<Arc<Mutex<vmlab_hv_devices::tpm::TpmTis>>>>,
    pub(crate) smi_fwcfg: Mutex<Option<Arc<Mutex<crate::smm::SmiFwCfg>>>>,
    pending_smis: Mutex<std::collections::BTreeSet<u8>>,
    smi_delivery: Condvar,
    reset_description: Mutex<Option<vmlab_hv_proto::machine::MachineDescription>>,
    reset_state: Mutex<Option<crate::reset::ResetState>>,
    reset_pending: std::sync::atomic::AtomicBool,
    legacy_reset_control: Mutex<u8>,
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
            vfio: Mutex::new(Vec::new()),
            io_trace: std::env::var_os("VMLAB_HV_DIAGNOSTICS")
                .map(|_| Mutex::new(std::collections::VecDeque::new())),
            display: Mutex::new(None),
            display_generation: std::sync::atomic::AtomicU64::new(0),
            vga_mapping: Mutex::new(None),
            last_frame: Mutex::new(std::time::Instant::now()),
            memory_gate: Mutex::new(MemoryGate::default()),
            memory_cv: Condvar::new(),
            memory_writer: Mutex::new(()),
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
            mch: Mutex::new(None),
            chipset: Mutex::new(None),
            vga: Mutex::new(None),
            xhci: Mutex::new(None),
            usb_hotplug: std::sync::atomic::AtomicBool::new(false),
            usb_attachments: Mutex::new(Vec::new()),
            pflash: Mutex::new(None),
            tpm: Mutex::new(None),
            smi_fwcfg: Mutex::new(None),
            pending_smis: Mutex::new(std::collections::BTreeSet::new()),
            smi_delivery: Condvar::new(),
            reset_description: Mutex::new(None),
            reset_state: Mutex::new(None),
            reset_pending: std::sync::atomic::AtomicBool::new(false),
            legacy_reset_control: Mutex::new(0),
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

    pub(crate) fn set_reset_description(&self, desc: vmlab_hv_proto::machine::MachineDescription) {
        *self.reset_description.lock() = Some(desc);
    }

    #[track_caller]
    pub(crate) fn request_reset(&self) {
        if std::env::var_os("VMLAB_HV_DIAGNOSTICS").is_some() {
            eprintln!("reset requested at {}", std::panic::Location::caller());
        }
        if self
            .reset_description
            .lock()
            .as_ref()
            .is_none_or(|desc| desc.exit_on_reset)
        {
            self.request_exit(ExitReason::Shutdown(ShutdownReason::GuestReset));
            return;
        }
        self.reset_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.kick_all();
        self.state_cv.notify_all();
    }

    fn service_reset(&self) {
        let Some(_op) = self.snapshot_op.try_lock() else {
            return;
        };
        if !self.reset_pending.load(std::sync::atomic::Ordering::SeqCst) || self.exited() {
            return;
        }
        let result = (|| {
            let was = self.stop_for_snapshot(std::time::Duration::from_secs(10))?;
            self.reset_pending
                .store(false, std::sync::atomic::Ordering::SeqCst);
            if self.exited() {
                return Ok(());
            }
            let mut reset = self.reset_state.lock();
            reset
                .as_mut()
                .ok_or("machine has no reset state")?
                .reset(self, self.reset_description.lock().as_ref())?;
            if !self.exited() {
                self.emit(Event::Reset);
                self.finish_snapshot(if was == RunState::Paused {
                    crate::snapshot::format::RUN_PAUSED
                } else {
                    crate::snapshot::format::RUN_RUNNING
                });
            }
            Ok::<_, String>(())
        })();
        if let Err(error) = result {
            self.request_exit(ExitReason::Error(format!("system reset: {error}")));
        }
    }

    pub(crate) fn reset_platform(&self) {
        use vmlab_hv_devices::Resettable;
        let _ = self.pm.lock().reset();
        self.sleep_signal.take();
        self.pending_smis.lock().clear();
        *self.legacy_reset_control.lock() = 0;
        if let Some(chipset) = self.chipset.lock().as_ref() {
            chipset.power.lock().take();
            chipset.error.lock().take();
        }
    }

    // ---- host USB passthrough ------------------------------------------

    /// Whether hot-plug commands may run: the description asked for the
    /// `usbx` controller.
    pub(crate) fn usb_hotplug(&self) -> bool {
        self.usb_hotplug.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The first free root port for a device of `speed`: SuperSpeed
    /// devices belong on ports 1..4, others on 5..8.
    fn free_usb_port(
        &self,
        speed: UsbSpeed,
        controller: &vmlab_hv_devices::usb::XhciController,
    ) -> Option<u8> {
        let attached: std::collections::HashSet<u8> =
            self.usb_attachments.lock().iter().map(|a| a.port).collect();
        let candidates: &[u8] = if speed == UsbSpeed::Super {
            &[1, 2, 3, 4]
        } else {
            &[5, 6, 7, 8]
        };
        candidates
            .iter()
            .copied()
            .find(|&port| !attached.contains(&port) && controller.port_free(port))
    }

    /// Attach a host device built by `open`. The controller picks the root
    /// port by protocol; the id is vmlab's qdev id. Returns the guest port.
    pub(crate) fn attach_usb(
        &self,
        id: &str,
        hostbus: u16,
        hostaddr: u16,
        hostport: &str,
        open: &dyn Fn() -> Result<Box<dyn UsbDevice>, String>,
    ) -> Result<u8, (vmlab_hv_proto::control::ErrorCode, String)> {
        use vmlab_hv_proto::control::ErrorCode;
        if !self.usb_hotplug() {
            return Err((
                ErrorCode::Unsupported,
                "this machine has no USB passthrough controller: set usb = true and restart it"
                    .into(),
            ));
        }
        if self.usb_attachments.lock().iter().any(|a| a.id == id) {
            return Err((
                ErrorCode::Unsupported,
                format!("a USB device with id {id:?} is already attached"),
            ));
        }
        let Some(controller_arc) = self.xhci.lock().clone() else {
            return Err((ErrorCode::Internal, "the USB controller is gone".into()));
        };
        let mut controller = controller_arc.lock();
        // Open and take over the host device before touching the guest's
        // view: a claim failure returns the device to the host inside the
        // backend and never reaches the controller.
        let device = open().map_err(|message| (ErrorCode::Internal, message))?;
        let speed = device.speed();
        let Some(port) = self.free_usb_port(speed, &controller) else {
            return Err((
                ErrorCode::Unsupported,
                "no free USB root port for this device".into(),
            ));
        };
        controller
            .attach(port, device)
            .map_err(|e| (ErrorCode::Internal, e.to_string()))?;
        self.usb_attachments.lock().push(UsbAttachment {
            id: id.to_owned(),
            port,
            hostbus,
            hostaddr,
            hostport: hostport.to_owned(),
        });
        Ok(port)
    }

    /// Detach the device with qdev id `id`, returning it to the host: the
    /// backend's drop releases the interfaces and rebinds the host driver.
    pub(crate) fn detach_usb(&self, id: &str) -> Result<u8, vmlab_hv_proto::control::ErrorCode> {
        let Some(controller) = self.xhci.lock().clone() else {
            return Err(vmlab_hv_proto::control::ErrorCode::Internal);
        };
        let mut controller = controller.lock();
        let mut attachments = self.usb_attachments.lock();
        let Some(index) = attachments.iter().position(|a| a.id == id) else {
            return Err(vmlab_hv_proto::control::ErrorCode::DeviceNotFound);
        };
        let port = attachments.remove(index).port;
        let detached = controller.detach(port);
        drop(attachments);
        drop(controller);
        // Either the detach handed back the backend, whose drop rebinds the
        // host driver, or the port was already empty (host unplug drained
        // it); either way the device is back on the host.
        drop(detached);
        self.emit(Event::DeviceDeleted { id: id.to_owned() });
        Ok(port)
    }

    /// Detach attachments whose host device went away, emitting
    /// `device_deleted` for each. Called from the poll path.
    pub(crate) fn detach_lost_usb(&self) {
        let lost: Vec<UsbAttachment> = {
            let Some(controller) = self.xhci.lock().clone() else {
                return;
            };
            let ports = controller.lock().lost_ports();
            if ports.is_empty() {
                return;
            }
            self.usb_attachments
                .lock()
                .iter()
                .filter(|a| ports.contains(&a.port))
                .cloned()
                .collect()
        };
        for attachment in lost {
            let _ = self.detach_usb(&attachment.id);
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
        let tids = self.kicks.lock();
        for &tid in tids.iter() {
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
            && !self.reset_pending.load(std::sync::atomic::Ordering::SeqCst)
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
        while (*state != RunState::Running
            || self.reset_pending.load(std::sync::atomic::Ordering::SeqCst))
            && self.exit.lock().is_none()
        {
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
            // A signal delivered just before KVM_RUN can be consumed before
            // the ioctl blocks. Retry while waiting for every vCPU to park.
            self.kick_all();
            self.parked_cv
                .wait_for(&mut parked, std::time::Duration::from_millis(1));
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
            self.clock_running(false);
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
        self.display_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(chipset) = self.chipset.lock().as_ref() {
            chipset.sync_pirqs();
        }
        match run_code {
            crate::snapshot::format::RUN_RUNNING => {
                let mut state = self.state.lock();
                if *state == RunState::Paused {
                    *state = RunState::Running;
                    drop(state);
                    self.clock_running(true);
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
        self.clock_running(false);
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
                    self.clock_running(false);
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
        self.trace_io(port, data, false);
        if self.chipset.lock().is_none() && data.len() == 1 && matches!(port, 0xcf9 | 0x64) {
            data[0] = if port == 0xcf9 {
                *self.legacy_reset_control.lock()
            } else {
                0
            };
            return;
        }
        if matches!(port, 0x3b0..=0x3df | 0x1ce..=0x1d0)
            && let Some(vga) = self.vga.lock().as_ref()
        {
            vga.io_read(port, data);
            return;
        }
        if let Some(chipset) = self.chipset.lock().as_ref()
            && chipset.io_read(port, data)
        {
            return;
        }
        if port >= IO_ALLOC_BASE {
            let mut root = self.pci_root.lock();
            if root.dispatch_io_read(port, data) {
                return;
            }
        }
        self.io_bus.lock().read(port, data);
    }

    pub(crate) fn io_write(&self, port: u16, data: &[u8]) {
        self.trace_io(port, data, true);
        if self.chipset.lock().is_none() && data.len() == 1 {
            if port == 0xcf9 {
                let mut control = self.legacy_reset_control.lock();
                let reset = *control & 4 == 0 && data[0] & 4 != 0;
                *control = data[0] & 0x0e;
                drop(control);
                if reset {
                    self.request_reset();
                }
                return;
            }
            if port == 0x64 && data[0] == 0xfe {
                self.request_reset();
                return;
            }
        }
        let _memory_update = if (0xcfc..=0xcff).contains(&port) && self.mch.lock().is_some() {
            Some(self.memory_update())
        } else {
            None
        };
        if matches!(port, 0x3b0..=0x3df | 0x1ce..=0x1d0)
            && let Some(vga) = self.vga.lock().as_ref()
        {
            vga.io_write(port, data);
            return;
        }
        let handled = self
            .chipset
            .lock()
            .as_ref()
            .is_some_and(|chipset| chipset.io_write(port, data));
        if handled {
            self.poll_chipset();
            return;
        }
        if port >= IO_ALLOC_BASE {
            let mut root = self.pci_root.lock();
            if root.dispatch_io_write(port, data) {
                let error = if self.vfio.lock().is_empty() {
                    None
                } else {
                    root.take_error()
                };
                drop(root);
                if let Some(error) = error {
                    self.request_exit(ExitReason::Error(error));
                }
                self.take_guest_sleep();
                return;
            }
        }
        self.io_bus.lock().write(port, data);
        if !self.vfio.lock().is_empty()
            && let Some(error) = self.pci_root.lock().take_error()
        {
            self.request_exit(ExitReason::Error(error));
        }
        if _memory_update.is_some()
            && let Err(error) = self.sync_vga_mapping()
        {
            self.request_exit(ExitReason::Error(error.to_string()));
        }
        if let Some(chipset) = self.chipset.lock().as_ref() {
            chipset.sync_pirqs();
        }
        if let Some(mch) = self.mch.lock().as_ref()
            && let Some(error) = mch.lock().error.take()
        {
            self.request_exit(ExitReason::Error(error));
        }
        self.take_guest_sleep();
    }

    pub(crate) fn mmio_read(&self, addr: u64, data: &mut [u8]) {
        self.advance_chipset();
        if (0xa0000..0xc0000).contains(&addr)
            && let Some(vga) = self.vga.lock().as_ref()
        {
            vga.legacy_read(addr - 0xa0000, data);
            return;
        }
        if let Some(offset) = self.ecam_offset(addr) {
            use vmlab_hv_devices::BusDevice;
            crate::pci::PciEcam::new(self.pci_root.clone()).read(offset, data);
            return;
        }
        if self.in_pci_window(addr) {
            let mut root = self.pci_root.lock();
            if root.dispatch_mmio_read(addr, data) {
                return;
            }
        }
        self.mmio_bus.lock().read(addr, data);
    }
    pub(crate) fn mmio_write(&self, addr: u64, data: &[u8]) {
        self.advance_chipset();
        if (0xa0000..0xc0000).contains(&addr)
            && let Some(vga) = self.vga.lock().as_ref()
        {
            vga.legacy_write(addr - 0xa0000, data);
            return;
        }
        if let Some(offset) = self.ecam_offset(addr) {
            let _memory_update = self.memory_update();
            use vmlab_hv_devices::BusDevice;
            crate::pci::PciEcam::new(self.pci_root.clone()).write(offset, data);
            if !self.vfio.lock().is_empty()
                && let Some(error) = self.pci_root.lock().take_error()
            {
                self.request_exit(ExitReason::Error(error));
            }
            if let Err(error) = self.sync_vga_mapping() {
                self.request_exit(ExitReason::Error(error.to_string()));
            }
            if let Some(chipset) = self.chipset.lock().as_ref() {
                chipset.sync_pirqs();
            }
            return;
        }
        if self.in_pci_window(addr) {
            let mut root = self.pci_root.lock();
            if root.dispatch_mmio_write(addr, data) {
                let error = if self.vfio.lock().is_empty() {
                    None
                } else {
                    root.take_error()
                };
                drop(root);
                if let Some(error) = error {
                    self.request_exit(ExitReason::Error(error));
                }
                return;
            }
        }
        self.mmio_bus.lock().write(addr, data);
    }

    pub(crate) fn flash_address(&self, addr: u64) -> bool {
        (vmlab_hv_devices::pflash::BASE..1 << 32).contains(&addr) && self.pflash.lock().is_some()
    }

    pub(crate) fn write_flash(&self, addr: u64, data: &[u8], in_smm: bool) {
        if let Some(flash) = self.pflash.lock().as_ref()
            && let Err(e) =
                flash
                    .lock()
                    .write_access(addr - vmlab_hv_devices::pflash::BASE, data, in_smm)
        {
            self.request_exit(ExitReason::Error(e.to_string()));
        }
    }

    fn in_pci_window(&self, addr: u64) -> bool {
        if self.mch.lock().is_some() {
            return true;
        }
        let ((b32, e32), (b64, e64)) = self.pci_windows;
        (b32 <= addr && addr < e32) || (b64 <= addr && addr < e64) || !self.vfio.lock().is_empty()
    }

    pub(crate) fn sync_vga_mapping(&self) -> Result<(), VmmError> {
        use vm_memory::{GuestAddress, GuestMemoryBackend};
        let Some(vga) = self.vga.lock().clone() else {
            return Ok(());
        };
        let mut root = self.pci_root.lock();
        let address = root
            .addresses()
            .into_iter()
            .find(|&(slot, function)| root.config_read(0, slot, function, 0) == vga.pci_id());
        let base = address.and_then(|(slot, function)| {
            let decoded = root.config_read(0, slot, function, 1) & 2 != 0;
            root.save_bar_states()
                .into_iter()
                .find(|(devfn, _)| *devfn == (slot << 3 | function))
                .and_then(|(_, bars)| bars.first().copied())
                .filter(|(base, sizing)| {
                    decoded
                        && !sizing
                        && *base >= 0x80000000
                        && *base + vga.framebuffer_size() as u64 <= 0xfec00000
                })
                .map(|(base, _)| base)
        });
        let blob_base = address.and_then(|(slot, function)| {
            let decoded = root.config_read(0, slot, function, 1) & 2 != 0;
            root.save_bar_states()
                .into_iter()
                .find(|(devfn, _)| *devfn == (slot << 3 | function))
                .and_then(|(_, bars)| bars.get(2).copied())
                .filter(|(base, sizing)| decoded && !sizing && *base >= (1 << 32))
                .map(|(base, _)| base)
        });
        drop(root);
        vga.set_aperture_base(blob_base)?;
        let base = base.filter(|base| {
            self.ecam_offset(*base).is_none()
                && self
                    .ecam_offset(*base + vga.framebuffer_size() as u64 - 1)
                    .is_none()
        });
        let mut mapped = self.vga_mapping.lock();
        if *mapped == base {
            return Ok(());
        }
        let memory = vga.linear_framebuffer();
        let host = memory
            .get_host_address(GuestAddress(0))
            .map_err(|e| VmmError::Memory(e.to_string()))? as u64;
        for address_space in [0, 1 << 16] {
            if mapped.is_some() {
                let region = kvm_bindings::kvm_userspace_memory_region {
                    slot: address_space | 128,
                    ..Default::default()
                };
                // SAFETY: the caller has excluded all vCPUs through memory_update.
                unsafe { self.kvm.vm.set_user_memory_region(region) }
                    .map_err(|e| VmmError::ioctl("removing VGA framebuffer", e))?;
            }
            if let Some(base) = base {
                let region = kvm_bindings::kvm_userspace_memory_region {
                    slot: address_space | 128,
                    guest_phys_addr: base,
                    memory_size: vga.framebuffer_size() as u64,
                    userspace_addr: host,
                    flags: kvm_bindings::KVM_MEM_LOG_DIRTY_PAGES,
                };
                // SAFETY: VGA owns this page-aligned mapping for the VM lifetime;
                // all host accesses use volatile guest-memory operations.
                unsafe { self.kvm.vm.set_user_memory_region(region) }
                    .map_err(|e| VmmError::ioctl("mapping VGA framebuffer", e))?;
            }
        }
        *mapped = base;
        Ok(())
    }

    pub(crate) fn vga_framebuffer_dirty(&self) -> bool {
        self.vga_mapping.lock().is_some()
            && [128, (1 << 16) | 128].into_iter().any(|slot| {
                self.kvm
                    .vm
                    .get_dirty_log(slot, self.vga.lock().as_ref().unwrap().framebuffer_size())
                    .map_or(true, |bits| bits.into_iter().any(|word| word != 0))
            })
    }

    fn ecam_offset(&self, addr: u64) -> Option<u64> {
        let mch = self.mch.lock();
        let (base, size) = mch.as_ref()?.lock().ecam_window()?;
        addr.checked_sub(base).filter(|offset| *offset < size)
    }

    fn clock_running(&self, running: bool) {
        if let Some(chipset) = self.chipset.lock().as_ref() {
            chipset.set_running(running);
        }
    }

    fn advance_chipset(&self) {
        if let Some(chipset) = self.chipset.lock().as_ref() {
            chipset.advance();
        }
    }

    fn poll_display(&self) {
        let Some(mut last) = self.last_frame.try_lock() else {
            return;
        };
        if last.elapsed() < std::time::Duration::from_millis(16) {
            return;
        }
        if let Some(display) = self.display.lock().as_ref() {
            if let Err(e) = display.publish() {
                eprintln!("display frame: {e}");
            }
            if let Some(vga) = self.vga.lock().as_ref()
                && let Err(e) = display.set_cursor(vga.cursor())
            {
                eprintln!("display cursor: {e}");
            }
            if let Some(chipset) = self.chipset.lock().as_ref() {
                display.set_leds(chipset.ps2.lock().leds() as u32);
            }
        }
        *last = std::time::Instant::now();
    }

    /// Make batched flash writes durable outside the vCPUs and the event
    /// loop: a host sync can take tens of milliseconds under I/O load, and
    /// firmware programs thousands of NOR words while it boots.
    fn sync_flash_batch(&self) {
        let Some(flash) = self.pflash.lock().clone() else {
            return;
        };
        let handle = flash.lock().take_due_sync();
        let result = match handle {
            Ok(Some(file)) => file.sync_data().map_err(|error| {
                flash.lock().sync_failed();
                error.to_string()
            }),
            Ok(None) => Ok(()),
            Err(error) => Err(error.to_string()),
        };
        if let Err(error) = result {
            self.request_exit(ExitReason::Error(format!("pflash flush: {error}")));
        }
    }

    fn poll_chipset(&self) {
        self.poll_display();
        if self.status() == RunState::Running
            && let Some(xhci) = self.xhci.lock().as_ref()
        {
            xhci.lock().poll();
        }
        self.detach_lost_usb();
        let chipset = self.chipset.lock().clone();
        let Some(chipset) = chipset else { return };
        chipset.advance();
        let action = chipset.power.lock().take();
        use vmlab_hv_devices::chipset::lpc::PowerAction;
        match action {
            Some(PowerAction::PowerOff | PowerAction::Hibernate) => {
                self.request_exit(ExitReason::Shutdown(ShutdownReason::GuestShutdown))
            }
            Some(PowerAction::Reset) => self.request_reset(),
            Some(PowerAction::Suspend) => {
                *self.state.lock() = RunState::Suspended;
                self.clock_running(false);
                self.kick_all();
                self.emit(Event::Suspend);
            }
            None => {}
        }
        if let Some(error) = chipset.error.lock().take() {
            self.request_exit(ExitReason::Error(error));
        }
    }

    pub(crate) fn take_smi(&self) -> bool {
        self.chipset.lock().as_ref().is_some_and(|c| {
            c.smi_pending
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        })
    }

    pub(crate) fn dispatch_smi(&self, issuer: u8) -> bool {
        let broadcast = self
            .smi_fwcfg
            .lock()
            .as_ref()
            .is_some_and(|cfg| cfg.lock().broadcast());
        let targets: Vec<_> = self
            .vcpus
            .lock()
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| broadcast || *id == 0)
            .collect();
        self.pending_smis.lock().extend(targets.iter().copied());
        self.kick_all();
        targets.contains(&issuer)
    }

    pub(crate) fn take_pending_smi(&self, id: u8) -> bool {
        let removed = self.pending_smis.lock().remove(&id);
        self.smi_delivery.notify_all();
        removed
    }

    pub(crate) fn has_pending_smi(&self, id: u8) -> bool {
        self.pending_smis.lock().contains(&id)
    }

    pub(crate) fn wait_smi_delivery(&self) {
        // Queue the broadcast on every KVM vCPU before any target runs its
        // handler. Retry kicks that raced with entry into KVM_RUN.
        let mut pending = self.pending_smis.lock();
        while !pending.is_empty() && self.should_run() {
            self.kick_all();
            self.smi_delivery
                .wait_for(&mut pending, std::time::Duration::from_millis(1));
        }
    }

    pub(crate) fn save_pending_smis(&self) -> Vec<u8> {
        self.pending_smis.lock().iter().copied().collect()
    }

    pub(crate) fn restore_pending_smis(&self, targets: Vec<u8>) -> Result<(), String> {
        let vcpus = self.vcpus.lock();
        if targets
            .iter()
            .any(|target| !vcpus.iter().any(|(id, _)| id == target))
        {
            return Err("snapshot SMI targets an absent vCPU".into());
        }
        *self.pending_smis.lock() = targets.into_iter().collect();
        Ok(())
    }

    fn trace_io(&self, port: u16, data: &[u8], write: bool) {
        if let Some(trace) = &self.io_trace {
            let mut trace = trace.lock();
            if trace.len() == 12 {
                trace.pop_front();
            }
            trace.push_back(format!(
                "{} {port:#x} {data:x?}",
                if write { "out" } else { "in" }
            ));
        }
    }

    /// No CPU may execute through a temporarily removed PAM memslot. Exit
    /// KVM_RUN on every CPU before changing readonly permissions.
    fn memory_update(&self) -> MemoryUpdateGuard<'_> {
        let writer = self.memory_writer.lock();
        let mut gate = self.memory_gate.lock();
        gate.blocked = true;
        self.kick_all();
        while gate.in_kvm != 0 {
            // A signal can arrive just before KVM_RUN. Reassert the kick
            // while a CPU remains in flight, including APs waiting for SIPI.
            self.memory_cv
                .wait_for(&mut gate, std::time::Duration::from_millis(1));
            self.kick_all();
        }
        MemoryUpdateGuard {
            shared: self,
            _writer: writer,
        }
    }

    pub(crate) fn enter_kvm(&self) -> KvmRunGuard<'_> {
        let mut gate = self.memory_gate.lock();
        while gate.blocked {
            self.memory_cv.wait(&mut gate);
        }
        gate.in_kvm += 1;
        KvmRunGuard(self)
    }
}

/// A running machine: the shared state plus the vCPU threads.
pub struct Vm {
    shared: Arc<VmShared>,
    vcpus: Vec<VcpuThread>,
    /// Guest RAM, kept for snapshot orchestration and later vhost-user.
    pub ram: Arc<GuestRam>,
    timer: Option<std::thread::JoinHandle<()>>,
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
        *shared.reset_state.lock() = Some(
            crate::reset::ResetState::capture(&shared)
                .map_err(|e| VmmError::Other(format!("capturing power-on state: {e}")))?,
        );
        // Own each thread immediately, including during partial construction.
        let mut machine = Self {
            shared,
            vcpus: Vec::new(),
            ram,
            timer: None,
        };
        for (id, fd) in fds {
            machine.vcpus.push(
                VcpuThread::spawn(id, fd, machine.shared.clone())
                    .map_err(|e| VmmError::Other(format!("spawning vCPU threads: {e}")))?,
            );
        }
        machine.timer = {
            let weak = Arc::downgrade(&machine.shared);
            Some(
                std::thread::Builder::new()
                    .name("machine-events".into())
                    .spawn(move || {
                        while let Some(shared) = weak.upgrade() {
                            if shared.exited() {
                                break;
                            }
                            shared.poll_chipset();
                            shared.service_reset();
                            let mut state = shared.state.lock();
                            shared
                                .state_cv
                                .wait_for(&mut state, std::time::Duration::from_millis(1));
                        }
                    })
                    .map_err(|e| VmmError::Other(format!("spawning chipset timer: {e}")))?,
            )
        };
        if machine.shared.pflash.lock().is_some() {
            let weak = Arc::downgrade(&machine.shared);
            std::thread::Builder::new()
                .name("pflash-sync".into())
                .spawn(move || {
                    while let Some(shared) = weak.upgrade() {
                        if shared.exited() {
                            break;
                        }
                        shared.sync_flash_batch();
                        drop(shared);
                        std::thread::sleep(std::time::Duration::from_millis(4));
                    }
                })
                .map_err(|e| VmmError::Other(format!("spawning flash sync: {e}")))?;
        }
        Ok(machine)
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
        self.join_workers();
        if let Some(flash) = self.shared.pflash.lock().as_ref()
            && let Err(error) = flash.lock().flush()
        {
            return ExitReason::Error(format!("pflash flush at VM exit: {error}"));
        }
        for device in self.shared.vfio.lock().iter() {
            if let Err(error) = device.lock().reset() {
                return ExitReason::Error(format!("VFIO reset at VM exit: {error}"));
            }
        }
        if let Some(tpm) = self.shared.tpm.lock().as_ref()
            && let Err(e) = tpm.lock().shutdown()
        {
            return ExitReason::Error(format!("TPM shutdown: {e}"));
        }
        if std::env::var_os("VMLAB_HV_DIAGNOSTICS").is_some() {
            if let Some(mch) = self.shared.mch.lock().as_ref() {
                use vmlab_hv_devices::PciDevice;
                let mut mch = mch.lock();
                eprintln!(
                    "MCH SMRAM: {:x?}",
                    [
                        mch.config_read(0x50 / 4),
                        mch.config_read(0x9c / 4),
                        mch.config_read(0xac / 4)
                    ]
                );
            }
            for (id, vcpu) in self.shared.vcpus.lock().iter() {
                let fd = vcpu.lock();
                if let (Ok(regs), Ok(sregs)) = (fd.get_regs(), fd.get_sregs()) {
                    eprintln!(
                        "vcpu {id}: cs={:#x} rip={:#x} ax={:#x} dx={:#x} cr0={:#x}",
                        sregs.cs.base, regs.rip, regs.rax, regs.rdx, sregs.cr0
                    );
                    eprintln!("vcpu {id} MP state: {:?}", fd.get_mp_state());
                    if let Ok(address) = fd.translate_gva(sregs.cs.base.wrapping_add(regs.rip))
                        && address.valid != 0
                    {
                        use vm_memory::Bytes;
                        let mut code = [0; 64];
                        if self
                            .ram
                            .mem
                            .read_slice(
                                &mut code,
                                vm_memory::GuestAddress(address.physical_address),
                            )
                            .is_ok()
                        {
                            eprintln!("vcpu {id} instructions: {code:02x?}");
                        }
                    }
                }
                eprintln!(
                    "vcpu {id} SMI state: {:?}",
                    fd.get_vcpu_events().map(|e| e.smi)
                );
            }
            crate::diagnostics::dump_timer_state(&self.shared, &self.ram.mem);
            if let Some(trace) = &self.shared.io_trace {
                eprintln!("last IO: {:?}", trace.lock());
            }
            let mut root = self.shared.pci_root.lock();
            eprintln!("PCI BAR state: {:?}", root.save_bar_states());
            for (slot, function) in root.addresses() {
                if root.config_read(0, slot, function, 0) != 0x1001_1af4 {
                    continue;
                }
                let base = (root.config_read(0, slot, function, 8) as u64 & !15)
                    | ((root.config_read(0, slot, function, 9) as u64) << 32);
                let mut common = [0; 64];
                root.dispatch_mmio_read(base, &mut common);
                eprintln!("block common config: {common:x?}");
                let avail = u64::from_le_bytes(common[40..48].try_into().unwrap());
                let used = u64::from_le_bytes(common[48..56].try_into().unwrap());
                use vm_memory::Bytes;
                let mut rings = [0; 8];
                let _ = self
                    .ram
                    .mem
                    .read_slice(&mut rings[..4], vm_memory::GuestAddress(avail));
                let _ = self
                    .ram
                    .mem
                    .read_slice(&mut rings[4..], vm_memory::GuestAddress(used));
                eprintln!("block avail/used: {rings:x?}");
            }
            drop(root);
            if let Some(vga) = self.shared.vga.lock().as_ref() {
                let mut text = [0; 4000];
                vga.legacy_read(0x18000, &mut text);
                let text: Vec<_> = text
                    .iter()
                    .step_by(2)
                    .copied()
                    .filter(|b| *b != 0)
                    .collect();
                eprintln!(
                    "VGA text: {}",
                    String::from_utf8_lossy(&text)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
        }
        reason
    }

    fn join_workers(&mut self) {
        // A kick can arrive between the state check and KVM_RUN. Reassert it
        // until every CPU has left, including CPUs blocked in guest HLT.
        while self.vcpus.iter().any(|cpu| !cpu.is_finished()) {
            self.shared.kick_all();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        for cpu in self.vcpus.drain(..) {
            cpu.join();
        }
        if let Some(timer) = self.timer.take() {
            let _ = timer.join();
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        self.handle().quit();
        self.join_workers();
        if let Some(flash) = self.shared.pflash.lock().as_ref()
            && let Err(error) = flash.lock().flush()
        {
            eprintln!("pflash flush while dropping VM: {error}");
        }
        if let Some(display) = self.shared.display.lock().as_ref() {
            display.shutdown();
        }
    }
}

/// The control-server-facing view of a machine. Cheap to clone.
#[derive(Clone)]
pub struct VmHandle(Arc<VmShared>);

impl VmHandle {
    pub(crate) fn from_shared(shared: Arc<VmShared>) -> Self {
        Self(shared)
    }
    pub fn serve_display(
        &self,
        native: &std::path::Path,
        rfb: &std::path::Path,
    ) -> Result<(), VmmError> {
        crate::display::serve(&self.0, native, rfb)
    }
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
                self.0.clock_running(false);
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
                crate::diagnostics::resume_hook(&self.0);
                self.0.clock_running(true);
                if was == RunState::Suspended
                    && let Some(chipset) = self.0.chipset.lock().as_ref()
                {
                    chipset.lpc.lock().wakeup();
                }
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
        if let Some(chipset) = self.0.chipset.lock().as_ref() {
            chipset.lpc.lock().power_button();
        } else {
            self.0.pm.lock().press_power_button();
        }
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

    /// `device_add` for a `usb-host`: attach the host device at
    /// `hostbus`/`hostaddr` and return the guest root port.
    pub fn attach_usb(
        &self,
        id: &str,
        hostbus: u16,
        hostaddr: u16,
        hostport: &str,
        open: &dyn Fn() -> Result<Box<dyn UsbDevice>, String>,
    ) -> Result<u8, (vmlab_hv_proto::control::ErrorCode, String)> {
        self.0.attach_usb(id, hostbus, hostaddr, hostport, open)
    }

    /// `device_del`: detach the device with qdev id `id` back to the host.
    pub fn detach_usb(&self, id: &str) -> Result<u8, vmlab_hv_proto::control::ErrorCode> {
        self.0.detach_usb(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use vm_memory::{Bytes, GuestAddress};

    struct NullIrq;
    impl vmlab_hv_devices::IrqLine for NullIrq {
        fn set_level(&self, _: bool) {}
    }

    #[test]
    fn ordinary_io_does_not_scan_pci_errors_without_vfio() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use vmlab_hv_devices::{BarInfo, BarKind, PciDevice};
        struct Probe(Arc<AtomicUsize>);
        impl PciDevice for Probe {
            fn config_read(&mut self, _: usize) -> u32 {
                3
            }
            fn config_write(&mut self, _: usize, _: u64, _: &[u8]) {}
            fn bars(&self) -> Vec<BarInfo> {
                vec![BarInfo {
                    index: 0,
                    size: 4096,
                    kind: BarKind::Mmio32,
                }]
            }
            fn bar_read(&mut self, _: u8, _: u64, _: &mut [u8]) {}
            fn bar_write(&mut self, _: u8, _: u64, _: &[u8]) {}
            fn take_error(&mut self) -> Option<String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                None
            }
        }
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("SKIP VFIO I/O fast path: /dev/kvm absent");
            return;
        }
        let checks = Arc::new(AtomicUsize::new(0));
        let mut root = PciRoot::new(1 << 32);
        root.add_device(Arc::new(Mutex::new(Probe(checks.clone()))))
            .unwrap();
        let signal = Arc::new(SleepSignal::default());
        let shared = VmShared::new(
            KvmVm::new().unwrap(),
            IoBus::new(),
            MmioBus::new(),
            Arc::new(Mutex::new(root)),
            Arc::new(Mutex::new(PmDevice::new(Arc::new(NullIrq), signal.sink()))),
            signal,
            1 << 32,
        );
        for _ in 0..100 {
            shared.io_write(0x80, &[0]);
            shared.mmio_write(crate::layout::MMIO32_BASE, &[0]);
        }
        assert_eq!(checks.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn smi_delivery_waits_for_all_targets_and_preserves_queued_targets_on_pause() {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping SMI delivery test: /dev/kvm is unavailable");
            return;
        }
        let kvm = KvmVm::new().unwrap();
        let signal = Arc::new(SleepSignal::default());
        let shared = VmShared::new(
            kvm,
            IoBus::new(),
            MmioBus::new(),
            Arc::new(Mutex::new(PciRoot::new(1 << 32))),
            Arc::new(Mutex::new(PmDevice::new(Arc::new(NullIrq), signal.sink()))),
            signal,
            1 << 32,
        );
        for pause in [false, true] {
            *shared.state.lock() = RunState::Running;
            *shared.pending_smis.lock() = [0, 1].into_iter().collect();
            let (sent, received) = std::sync::mpsc::channel();
            let waiter = shared.clone();
            let thread = std::thread::spawn(move || {
                waiter.wait_smi_delivery();
                sent.send(()).unwrap();
            });
            assert!(received.recv_timeout(Duration::from_millis(10)).is_err());
            assert!(shared.take_pending_smi(0));
            assert!(received.recv_timeout(Duration::from_millis(10)).is_err());
            if pause {
                *shared.state.lock() = RunState::Paused;
            } else {
                assert!(shared.take_pending_smi(1));
            }
            received.recv_timeout(Duration::from_secs(1)).unwrap();
            thread.join().unwrap();
            assert_eq!(
                shared.save_pending_smis(),
                if pause { vec![1] } else { vec![] }
            );
        }
    }

    #[test]
    fn display_publication_without_clients_is_rate_limited() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use vmlab_hv_display::{DisplayServer, Frame, FrameSource, InputSink, Pixels};

        struct Probe(AtomicUsize);
        impl FrameSource for Probe {
            fn frame(&self) -> std::io::Result<Frame> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Frame {
                    generation: 0,
                    width: 1,
                    height: 1,
                    stride: 4,
                    drm_fourcc: vmlab_hv_display::protocol::XRGB8888,
                    modifier: 0,
                    timestamp_ns: 0,
                    damage: vec![],
                    pixels: Pixels::Shm(Arc::new(vec![0; 4])),
                })
            }
            fn readback(&self, _: &Frame) -> std::io::Result<Vec<u8>> {
                Ok(vec![0; 4])
            }
        }
        impl InputSink for Probe {
            fn input(&self, _: vmlab_hv_display::Message) {}
        }

        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping display publication test: /dev/kvm is unavailable");
            return;
        }
        let signal = Arc::new(SleepSignal::default());
        let shared = VmShared::new(
            KvmVm::new().unwrap(),
            IoBus::new(),
            MmioBus::new(),
            Arc::new(Mutex::new(PciRoot::new(1 << 32))),
            Arc::new(Mutex::new(PmDevice::new(Arc::new(NullIrq), signal.sink()))),
            signal,
            1 << 32,
        );
        let probe = Arc::new(Probe(AtomicUsize::new(0)));
        *shared.display.lock() = Some(Arc::new(
            DisplayServer::new(probe.clone(), probe.clone()).unwrap(),
        ));
        *shared.last_frame.lock() = std::time::Instant::now() - Duration::from_millis(16);
        shared.poll_chipset();
        assert_eq!(probe.0.load(Ordering::SeqCst), 2);
        // Keep the interval deterministic even if the test thread is descheduled.
        *shared.last_frame.lock() = std::time::Instant::now() + Duration::from_secs(1);
        shared.poll_chipset();
        assert_eq!(probe.0.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn quiesce_recovers_a_kick_lost_before_entering_kvm() {
        lost_kick(false);
    }

    #[test]
    fn shutdown_recovers_a_kick_lost_before_entering_kvm() {
        lost_kick(true);
    }

    fn lost_kick(shutdown: bool) {
        if !std::path::Path::new("/dev/kvm").exists() {
            return;
        }
        let kvm = KvmVm::new().unwrap();
        let layout = crate::layout::MemLayout::new(16).unwrap();
        let ram = GuestRam::anonymous(layout.clone()).unwrap();
        crate::memory::register_memslots(&kvm.vm, &ram).unwrap();
        ram.mem
            .write_slice(&[0xfa, 0xf4, 0xeb, 0xfd], GuestAddress(0x8000))
            .unwrap();
        let signal = Arc::new(SleepSignal::default());
        let pm = Arc::new(Mutex::new(PmDevice::new(Arc::new(NullIrq), signal.sink())));
        let shared = VmShared::new(
            kvm,
            IoBus::new(),
            MmioBus::new(),
            Arc::new(Mutex::new(PciRoot::new(layout.mmio64_base))),
            pm,
            signal,
            layout.mmio64_base,
        );
        let mut cpu = shared.kvm.create_vcpu(0).unwrap();
        crate::loader::init_real_mode(&mut cpu, 0x8000).unwrap();
        let mut vm = Vm::from_parts(shared.clone(), vec![(0, cpu)], ram).unwrap();
        vm.start();
        std::thread::sleep(Duration::from_millis(50));
        if shutdown {
            // Record quit without a pending signal, modeling an exit kick
            // consumed just before the blocked KVM_RUN.
            *shared.state.lock() = RunState::ShuttingDown;
            *shared.exit.lock() = Some(ExitReason::Shutdown(ShutdownReason::HostRequest));
            let start = std::time::Instant::now();
            vm.wait();
            assert!(start.elapsed() < Duration::from_millis(250));
            return;
        }
        // Model a kick consumed between the run-state check and KVM_RUN:
        // the vCPU is blocked in HLT, with a paused state and no pending signal.
        *shared.state.lock() = RunState::Paused;
        let result = shared.wait_quiesced(Duration::from_millis(250));
        vm.handle().quit();
        vm.wait();
        assert!(result.is_ok(), "{result:?}");
    }
}
