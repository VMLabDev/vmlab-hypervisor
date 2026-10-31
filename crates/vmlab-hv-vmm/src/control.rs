//! The control-socket side: mapping protocol commands onto a machine and
//! serving the socket with the protocol crate's server.

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;

use vmlab_hv_proto::control::{Capability, Command, ErrorCode, Outcome, RunState};
use vmlab_hv_proto::server::Server;

use crate::error::VmmError;
use crate::vm::VmHandle;

/// Maps control [`Command`]s to the machine. Protocol mechanics (handshake,
/// framing, reply ids) live in `vmlab-hv-proto`'s server; this is the VMM's
/// policy.
pub struct ControlHandler {
    vm: VmHandle,
    /// Builds the backend for one attach: the real path opens the usbdevfs
    /// node; tests substitute a fake device.
    usb_opener: UsbOpener,
}

/// What an attach opens: (hostbus, hostaddr) to a USB backend.
pub type UsbOpener = Arc<
    dyn Fn(u16, u16) -> Result<Box<dyn vmlab_hv_devices::usb::UsbDevice>, String> + Send + Sync,
>;

/// The production opener: open `/dev/bus/usb/<bus>/<dev>` and take over its
/// interfaces (detaching the host driver; a claim failure returns it).
fn open_usb_host(bus: u16, dev: u16) -> Result<Box<dyn vmlab_hv_devices::usb::UsbDevice>, String> {
    let host = vmlab_hv_devices::usb::HostUsbDevice::open(format!("{bus:03}-{dev:03}"), bus, dev)
        .map_err(|e| e.to_string())?;
    Ok(Box::new(host))
}

impl ControlHandler {
    pub fn new(vm: VmHandle) -> Self {
        ControlHandler {
            vm,
            usb_opener: Arc::new(open_usb_host),
        }
    }

    /// A handler whose attach path builds its devices through `opener`.
    pub fn with_usb_opener(vm: VmHandle, opener: UsbOpener) -> Self {
        ControlHandler {
            vm,
            usb_opener: opener,
        }
    }

    pub fn handle(&self, command: Command) -> Outcome {
        match command {
            Command::Status => Outcome::Ok {
                status: Some(self.vm.status()),
            },
            Command::Resume => match self.vm.resume() {
                Ok(state) => Outcome::Ok {
                    status: Some(state),
                },
                Err(message) => Outcome::Error {
                    code: ErrorCode::InvalidState,
                    message,
                },
            },
            Command::Pause => match self.vm.pause() {
                Ok(state) => Outcome::Ok {
                    status: Some(state),
                },
                Err(message) => Outcome::Error {
                    code: ErrorCode::InvalidState,
                    message,
                },
            },
            Command::Powerdown => match self.vm.powerdown() {
                Ok(state) => Outcome::Ok {
                    status: Some(state),
                },
                Err(message) => Outcome::Error {
                    code: ErrorCode::InvalidState,
                    message,
                },
            },
            Command::Quit => {
                self.vm.quit();
                Outcome::Ok {
                    status: Some(RunState::ShuttingDown),
                }
            }
            Command::DeviceAdd {
                id,
                driver,
                bus,
                hostbus,
                hostaddr,
                hostport,
            } => {
                if driver != "usb-host" {
                    return Outcome::Error {
                        code: ErrorCode::Unsupported,
                        message: format!("device_add supports usb-host, not {driver:?}"),
                    };
                }
                if let Some(bus) = bus.as_deref()
                    && bus != "usbx.0"
                {
                    return Outcome::Error {
                        code: ErrorCode::Unsupported,
                        message: format!("device_add supports the usbx.0 bus, not {bus:?}"),
                    };
                }
                let (hostbus, hostaddr) = match (u16::try_from(hostbus), u16::try_from(hostaddr)) {
                    (Ok(bus), Ok(addr)) => (bus, addr),
                    _ => {
                        return Outcome::Error {
                            code: ErrorCode::Unsupported,
                            message: format!(
                                "hostbus {hostbus} or hostaddr {hostaddr} out of range"
                            ),
                        };
                    }
                };
                let opener = self.usb_opener.clone();
                match self.vm.attach_usb(&id, hostbus, hostaddr, &hostport, &|| {
                    (opener)(hostbus, hostaddr)
                }) {
                    Ok(_port) => Outcome::Ok { status: None },
                    Err((code, message)) => Outcome::Error { code, message },
                }
            }
            Command::DeviceDel { id } => match self.vm.detach_usb(&id) {
                Ok(_port) => Outcome::Ok { status: None },
                Err(code) => Outcome::Error {
                    code,
                    message: format!("no attached USB device with id {id:?}"),
                },
            },
            Command::SnapshotSave { name } => {
                snapshot_outcome(self.vm.status(), crate::snapshot::save(&self.vm, &name))
            }
            Command::SnapshotLoad { name } => {
                snapshot_outcome(self.vm.status(), crate::snapshot::load(&self.vm, &name))
            }
            Command::SnapshotDelete { name } => {
                snapshot_outcome(self.vm.status(), crate::snapshot::delete(&self.vm, &name))
            }
        }
    }
}

/// Map a snapshot operation onto a protocol outcome.
fn snapshot_outcome(
    status: RunState,
    result: Result<(), crate::snapshot::SnapshotError>,
) -> Outcome {
    match result {
        Ok(()) => Outcome::Ok {
            status: Some(status),
        },
        Err(e) => Outcome::Error {
            code: match &e {
                crate::snapshot::SnapshotError::Unsupported(_) => ErrorCode::Unsupported,
                crate::snapshot::SnapshotError::NotFound(_) => ErrorCode::SnapshotNotFound,
                crate::snapshot::SnapshotError::Incompatible(_) => ErrorCode::SnapshotIncompatible,
                crate::snapshot::SnapshotError::Internal(_) => ErrorCode::Internal,
            },
            message: e.to_string(),
        },
    }
}

/// Bind the control socket and start serving it on its own thread. Machine
/// events are broadcast to every connected client through the returned
/// wiring; the caller keeps the machine handle.
pub fn serve(path: &Path, vm: &VmHandle) -> Result<std::thread::JoinHandle<()>, VmmError> {
    if path.exists() {
        return Err(VmmError::Control(format!(
            "control socket path {} already exists",
            path.display()
        )));
    }
    let listener = UnixListener::bind(path)
        .map_err(|e| VmmError::Control(format!("binding {}: {e}", path.display())))?;

    let weak = Arc::downgrade(vm.shared());
    let mut capabilities = vec![Capability::Snapshots];
    if vm.shared().vga.lock().is_some() {
        capabilities.extend([Capability::Display, Capability::Input]);
    }
    if vm.shared().usb_hotplug() {
        capabilities.push(Capability::Hotplug);
    }
    let server: Server<_> =
        Server::new(
            env!("CARGO_PKG_VERSION"),
            capabilities,
            move |command| match weak.upgrade() {
                Some(shared) => ControlHandler::new(VmHandle::from_shared(shared)).handle(command),
                None => Outcome::Error {
                    code: ErrorCode::InvalidState,
                    message: "machine has exited".into(),
                },
            },
        );
    let broadcaster = server.clone();
    vm.set_event_sink(Arc::new(move |event| broadcaster.broadcast(event)));

    let weak = Arc::downgrade(vm.shared());
    std::thread::Builder::new()
        .name("control-serve".to_string())
        .spawn(move || {
            if let Err(e) = server.serve_until(listener, || {
                weak.upgrade().is_none_or(|shared| shared.exited())
            }) {
                eprintln!("control: accept loop ended: {e}");
            }
        })
        .map_err(|e| VmmError::Control(format!("spawning control thread: {e}")))
}

#[cfg(test)]
mod usb_tests {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use vmlab_hv_devices::MsiMessage;
    use vmlab_hv_devices::usb::{SetupPacket, UsbDevice, UsbSpeed, UsbTransfer, XhciController};
    use vmlab_hv_proto::control::{Command, Event, Outcome};

    use super::*;
    use crate::bus::{IoBus, MmioBus};
    use crate::kvm::KvmVm;
    use crate::memory::GuestRam;
    use crate::pci::PciRoot;
    use crate::pm::{PmDevice, SleepSignal};
    use crate::vm::VmShared;

    struct NullIrq;
    impl vmlab_hv_devices::IrqLine for NullIrq {
        fn set_level(&self, _high: bool) {}
    }

    /// A host-backed device that reports itself lost when its flag is set.
    struct FakeHostDevice {
        lost: Arc<StdMutex<bool>>,
    }

    impl UsbDevice for FakeHostDevice {
        fn speed(&self) -> UsbSpeed {
            UsbSpeed::High
        }
        fn reset(&mut self) {}
        fn control(&mut self, _setup: SetupPacket, _data: &[u8]) -> UsbTransfer {
            UsbTransfer::Ack(Vec::new())
        }
        fn transfer(&mut self, _endpoint: u8, _data: &[u8], _max_length: usize) -> UsbTransfer {
            UsbTransfer::Nak
        }
        fn is_lost(&self) -> bool {
            *self.lost.lock().unwrap()
        }
        fn is_host_backed(&self) -> bool {
            true
        }
    }

    impl vmlab_hv_devices::Snapshot for FakeHostDevice {
        fn state_id(&self) -> String {
            "fake-host".into()
        }
        fn save_state(&self) -> Result<Vec<u8>, vmlab_hv_devices::DeviceError> {
            Err(vmlab_hv_devices::DeviceError::State(
                "host device snapshots refused".into(),
            ))
        }
        fn restore_state(&mut self, _bytes: &[u8]) -> Result<(), vmlab_hv_devices::DeviceError> {
            Err(vmlab_hv_devices::DeviceError::State(
                "host device snapshots refused".into(),
            ))
        }
    }

    #[derive(Default)]
    struct Sinks(StdMutex<Vec<MsiMessage>>);
    impl vmlab_hv_devices::MsiSink for Sinks {
        fn signal(&self, message: MsiMessage) {
            self.0.lock().unwrap().push(message);
        }
    }

    /// One machine: a shared VM with an xHCI and the hot-plug flag set, a
    /// control handler with a fake opener, and an event collector.
    struct Fixture {
        handler: ControlHandler,
        handle: crate::vm::VmHandle,
        events: Arc<StdMutex<Vec<Event>>>,
        lost: Arc<StdMutex<bool>>,
        shared: Arc<VmShared>,
        _vm: crate::vm::Vm,
    }

    fn kvm_or_skip() -> Option<KvmVm> {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping: /dev/kvm is not available");
            return None;
        }
        match KvmVm::new() {
            Ok(vm) => Some(vm),
            Err(e) => {
                eprintln!("skipping: cannot create KVM VM: {e}");
                None
            }
        }
    }

    fn fixture() -> Option<Fixture> {
        let kvm = kvm_or_skip()?;
        let layout = crate::layout::MemLayout::new(16).ok()?;
        let ram = GuestRam::anonymous(layout.clone()).ok()?;
        crate::memory::register_memslots(&kvm.vm, &ram).ok()?;
        let pci_root = Arc::new(parking_lot::Mutex::new(PciRoot::new(layout.mmio64_base)));
        let sleep_signal = Arc::new(SleepSignal::default());
        let pm = Arc::new(parking_lot::Mutex::new(PmDevice::new(
            Arc::new(NullIrq),
            sleep_signal.sink(),
        )));
        let shared = VmShared::new(
            kvm,
            IoBus::new(),
            MmioBus::new(),
            pci_root,
            pm,
            sleep_signal,
            layout.mmio64_base,
        );
        let controller = Arc::new(parking_lot::Mutex::new(XhciController::new(
            "xhci",
            ram.mem.clone(),
            Arc::new(Sinks::default()),
            None,
        )));
        *shared.xhci.lock() = Some(controller);
        shared
            .usb_hotplug
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let events: Arc<StdMutex<Vec<Event>>> = Arc::default();
        shared.set_event_sink({
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        });
        let vm = crate::vm::Vm::from_parts(shared.clone(), Vec::new(), ram).ok()?;
        let lost: Arc<StdMutex<bool>> = Arc::default();
        let opener_lost = lost.clone();
        let handler = ControlHandler::with_usb_opener(
            vm.handle(),
            Arc::new(move |_bus, _dev| {
                Ok(Box::new(FakeHostDevice {
                    lost: opener_lost.clone(),
                }) as Box<dyn UsbDevice>)
            }),
        );
        Some(Fixture {
            handler,
            handle: vm.handle(),
            events,
            lost,
            shared: vm.handle().shared().clone(),
            _vm: vm,
        })
    }

    fn device_add(id: &str, driver: &str, bus: Option<&str>) -> Command {
        Command::DeviceAdd {
            id: id.to_owned(),
            driver: driver.to_owned(),
            bus: bus.map(str::to_owned),
            hostbus: 1,
            hostaddr: 5,
            hostport: "2".to_owned(),
        }
    }

    #[test]
    fn attach_and_detach_round_trip_with_events() {
        let Some(f) = fixture() else { return };
        // The device lands on a USB2 root port past the tablet.
        match f
            .handler
            .handle(device_add("usbhost-1-5", "usb-host", Some("usbx.0")))
        {
            Outcome::Ok { .. } => {}
            other => panic!("attach failed: {other:?}"),
        }
        let attachments = f.shared.usb_attachments.lock().clone();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].id, "usbhost-1-5");
        assert_eq!(attachments[0].port, 6);
        assert_eq!(attachments[0].hostport, "2");
        assert!(
            f.shared
                .xhci
                .lock()
                .as_ref()
                .unwrap()
                .lock()
                .has_host_devices()
        );

        // Duplicate ids and unknown drivers/buses are refused.
        assert!(matches!(
            f.handler
                .handle(device_add("usbhost-1-5", "usb-host", Some("usbx.0"))),
            Outcome::Error {
                code: ErrorCode::Unsupported,
                ..
            }
        ));
        assert!(matches!(
            f.handler.handle(device_add("x", "vfio-pci", None)),
            Outcome::Error {
                code: ErrorCode::Unsupported,
                ..
            }
        ));
        assert!(matches!(
            f.handler
                .handle(device_add("x", "usb-host", Some("other.0"))),
            Outcome::Error {
                code: ErrorCode::Unsupported,
                ..
            }
        ));

        // An online snapshot is refused while the device is attached (D9):
        // the refusal comes before any registry or run-state work.
        let saved = crate::snapshot::save(&f.handle, "nope");
        assert!(matches!(
            saved,
            Err(crate::snapshot::SnapshotError::Unsupported(ref message))
                if message.contains("host USB")
        ));

        // Detach: the reply is ok and a device_deleted event follows.
        assert!(matches!(
            f.handler.handle(Command::DeviceDel {
                id: "usbhost-1-5".into()
            }),
            Outcome::Ok { .. }
        ));
        assert!(f.shared.usb_attachments.lock().is_empty());
        assert!(
            !f.shared
                .xhci
                .lock()
                .as_ref()
                .unwrap()
                .lock()
                .has_host_devices()
        );
        assert_eq!(
            *f.events.lock().unwrap(),
            vec![Event::DeviceDeleted {
                id: "usbhost-1-5".into()
            }]
        );
        // Unknown ids say so.
        assert!(matches!(
            f.handler.handle(Command::DeviceDel { id: "ghost".into() }),
            Outcome::Error {
                code: ErrorCode::DeviceNotFound,
                ..
            }
        ));
    }

    #[test]
    fn a_machine_without_hotplug_refuses_attaches() {
        let Some(f) = fixture() else { return };
        f.shared
            .usb_hotplug
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            f.handler.handle(device_add("usbhost-1-5", "usb-host", Some("usbx.0"))),
            Outcome::Error { code: ErrorCode::Unsupported, ref message } if message.contains("usb = true")
        ));
    }

    #[test]
    fn concurrent_detaches_emit_one_event() {
        let Some(f) = fixture() else { return };
        assert!(matches!(
            f.handler
                .handle(device_add("usbhost-1-5", "usb-host", Some("usbx.0"))),
            Outcome::Ok { .. }
        ));
        let controller = f.shared.xhci.lock().clone().unwrap();
        let held = controller.lock();
        let start = Arc::new(std::sync::Barrier::new(17));
        let workers: Vec<_> = (0..16)
            .map(|_| {
                let shared = f.shared.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    shared.detach_usb("usbhost-1-5")
                })
            })
            .collect();
        start.wait();
        std::thread::sleep(std::time::Duration::from_millis(25));
        drop(held);
        let detached = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(Result::is_ok)
            .count();
        let events = f.events.lock().unwrap().clone();
        assert_eq!(detached, 1);
        assert_eq!(
            events,
            vec![Event::DeviceDeleted {
                id: "usbhost-1-5".into()
            }]
        );
    }

    #[test]
    fn a_lost_host_device_detaches_with_an_event() {
        let Some(f) = fixture() else { return };
        assert!(matches!(
            f.handler
                .handle(device_add("usbhost-1-5", "usb-host", Some("usbx.0"))),
            Outcome::Ok { .. }
        ));
        // The host device disappears.
        *f.lost.lock().unwrap() = true;
        f.shared.detach_lost_usb();
        // The event worker can win the detach and still be publishing its
        // notification when this poll returns.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let events = loop {
            let events = f.events.lock().unwrap().clone();
            if !events.is_empty() {
                break events;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no device_deleted event"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert!(f.shared.usb_attachments.lock().is_empty());
        assert_eq!(
            events,
            vec![Event::DeviceDeleted {
                id: "usbhost-1-5".into()
            }]
        );
    }
}
