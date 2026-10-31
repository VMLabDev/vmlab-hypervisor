// Copyright 2019 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE.crosvm file.

//! Host USB passthrough backend: a device on the host's usbdevfs behind the
//! xHCI's `UsbDevice` seam.
//!
//! Lifted from crosvm's `devices/src/usb/backend` host backend and
//! `usb_util`, pruned of FIDO and of crosvm's event-loop/job-queue plumbing:
//! this controller is polled (1 ms), so URB completions are reaped inside
//! `control`/`transfer` retries instead of by a handler thread. The lift
//! source never rebinds host kernel drivers after releasing an interface
//! (notes/03 F23); this backend issues USBDEVFS_CONNECT on detach and on
//! claim failure so the device returns to the host.

pub mod descriptor;
pub mod device;
pub mod error;
pub mod fs;
pub mod sys;
pub mod types;

#[cfg(test)]
pub mod fake;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use descriptor::ConfigDescriptorTree;
pub use device::{Device, Transfer, TransferStatus};
pub use error::{Error, Result};
pub use fs::{UsbFs, Usbdevfs};
use types::{
    ControlRequestDataPhaseTransferDirection, ControlRequestRecipient, DeviceSpeed, EndpointType,
    StandardControlRequest,
};
use zerocopy::IntoBytes;

use crate::usb::{SetupPacket, UsbDevice, UsbSpeed, UsbTransfer};
use crate::{DeviceError, Snapshot};

/// Standard ENDPOINT_HALT feature selector cleared to clear a stall.
const STD_FEATURE_ENDPOINT_HALT: u16 = 0;
/// Descriptor rewriting target for interfaces the guest may not drive.
const VENDOR_SPECIFIC: u8 = 0xff;
/// Where the reattached device node lives.
const USB_DEV_ROOT: &str = "/dev/bus/usb";

#[derive(Clone, Copy)]
struct EndpointInfo {
    ty: EndpointType,
    max_packet: u16,
}

/// What one in-flight or completed transfer means to the guest.
enum Pending {
    InFlight,
    Done(UsbTransfer),
}

/// A host USB device passed through to the guest. One instance owns the
/// usbdevfs file, its interface claims, and the in-flight URBs.
pub struct HostUsbDevice {
    id: String,
    device: Device,
    /// Whether the guest has issued its first SET_CONFIGURATION.
    initialized: bool,
    alt_settings: HashMap<u8, u8>,
    claimed_interfaces: Vec<u8>,
    endpoints: HashMap<u8, EndpointInfo>,
    /// Per-endpoint transfer state, keyed by USB endpoint address
    /// (0 for the control endpoint).
    pending: HashMap<u8, Pending>,
    /// Endpoints whose in-flight work was dropped; their cancelled
    /// completions are discarded rather than delivered.
    discarded: Vec<u8>,
}

impl HostUsbDevice {
    /// Open `/dev/bus/usb/<bus>/<dev>` and take over its interfaces.
    pub fn open(id: impl Into<String>, bus: u16, dev: u16) -> Result<Self> {
        let path = device_node(bus, dev);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| Error::Other(format!("opening {}: {e}", path.display())))?;
        Self::new(id, Box::new(Usbdevfs::new(file)))
    }

    /// Build the backend over an open usbdevfs (or a fake of one).
    pub fn new(id: impl Into<String>, fs: Box<dyn UsbFs>) -> Result<Self> {
        let mut this = Self {
            id: id.into(),
            device: Device::new(fs)?,
            initialized: false,
            alt_settings: HashMap::new(),
            claimed_interfaces: Vec::new(),
            endpoints: HashMap::new(),
            pending: HashMap::new(),
            discarded: Vec::new(),
        };
        // Claim the interfaces of the active configuration, if the device
        // is configured; a freshly plugged device may still be at
        // configuration 0 and is claimed when the guest configures it.
        if let Ok(config) = this.device.get_active_configuration()
            && config != 0
            && let Ok(descriptor) = this.device.get_config_descriptor(config)
        {
            this.claim_interfaces(&descriptor)?;
            this.rebuild_endpoints(&descriptor)?;
        }
        Ok(this)
    }

    /// "vendor:product" for logs and refusal messages.
    pub fn describe(&self) -> String {
        let descriptor = self.device.get_device_descriptor();
        // Copy the packed fields before formatting borrows them.
        let (vendor, product) = (descriptor.idVendor, descriptor.idProduct);
        format!("{vendor:04x}:{product:04x}")
    }

    fn usb_speed(&self) -> UsbSpeed {
        match self.device.get_speed().unwrap_or(None) {
            Some(DeviceSpeed::Low) => UsbSpeed::Low,
            Some(DeviceSpeed::Full) => UsbSpeed::Full,
            Some(DeviceSpeed::Super | DeviceSpeed::SuperPlus) => UsbSpeed::Super,
            _ => UsbSpeed::High,
        }
    }

    /// Reap completed URBs and file them under their endpoints.
    fn reap(&mut self) {
        let completed = match self.device.poll_transfers() {
            Ok(completed) => completed,
            Err(_) => return,
        };
        for transfer in completed {
            let endpoint = transfer.endpoint();
            match transfer.status() {
                TransferStatus::Cancelled if self.discarded.contains(&endpoint) => {
                    self.discarded.retain(|ep| *ep != endpoint);
                }
                status => {
                    let outcome = match status {
                        TransferStatus::Completed => {
                            let isochronous = self
                                .endpoints
                                .get(&endpoint)
                                .is_some_and(|ep| ep.ty == EndpointType::Isochronous);
                            if endpoint == 0 {
                                UsbTransfer::Ack(transfer.control_data().to_vec())
                            } else if isochronous && endpoint & 0x80 != 0 {
                                UsbTransfer::Ack(transfer.iso_data())
                            } else if endpoint & 0x80 != 0 {
                                UsbTransfer::Ack(transfer.data().to_vec())
                            } else {
                                UsbTransfer::Ack(Vec::new())
                            }
                        }
                        _ => UsbTransfer::Stall,
                    };
                    self.pending.insert(endpoint, Pending::Done(outcome));
                }
            }
        }
    }

    /// Start a transfer unless one is already in flight; `Nak` keeps the
    /// guest's TD queued until a retry finds the completion.
    fn start(&mut self, endpoint: u8, transfer: Transfer) -> UsbTransfer {
        match self.device.submit_transfer(transfer) {
            Ok(()) => {
                self.pending.insert(endpoint, Pending::InFlight);
                UsbTransfer::Nak
            }
            Err(_) => UsbTransfer::Stall,
        }
    }

    fn take_pending(&mut self, endpoint: u8) -> UsbTransfer {
        match self.pending.remove(&endpoint) {
            Some(Pending::Done(outcome)) => outcome,
            _ => UsbTransfer::Nak,
        }
    }

    /// Claims every interface of `config_descriptor`, detaching whatever
    /// host kernel driver is bound to each. Either all of them are claimed
    /// or none are; on failure the detached interfaces are reconnected so
    /// the host's drivers rebind.
    fn claim_interfaces(&mut self, config_descriptor: &ConfigDescriptorTree) -> Result<()> {
        for i in 0..config_descriptor.bNumInterfaces {
            // Bound to a variable, not matched on directly: the Err arm
            // below releases and reconnects, which must not hold a borrow.
            let claimed = self.device.claim_interface(i);
            match claimed {
                Ok(()) => {
                    self.claimed_interfaces.push(i);
                }
                Err(e) => {
                    self.release_and_reconnect();
                    return Err(Error::ClaimInterface(i, Box::new(e)));
                }
            }
        }
        Ok(())
    }

    /// Release every claimed interface and let host kernel drivers bind
    /// again (USBDEVFS_CONNECT). This is the detach path and the
    /// claim-failure path; a guest-driven reconfiguration only releases,
    /// because the interfaces are about to be claimed again.
    fn release_and_reconnect(&mut self) {
        for i in &self.claimed_interfaces {
            // Errors are expected when the device is already gone.
            let _ = self.device.release_interface(*i);
        }
        for i in &self.claimed_interfaces {
            // Errors are expected when the device is already gone.
            let _ = self.device.connect_interface(*i);
        }
        self.claimed_interfaces.clear();
        self.endpoints.clear();
    }

    fn release_interfaces(&mut self) {
        for i in &self.claimed_interfaces {
            // Errors are expected when the device is already gone.
            let _ = self.device.release_interface(*i);
        }
        self.claimed_interfaces.clear();
        self.endpoints.clear();
    }

    fn set_config(&mut self, config: u8) -> Result<()> {
        self.release_interfaces();
        let descriptor = if config == 0 {
            None
        } else {
            Some(self.device.get_config_descriptor(config)?)
        };

        let mut need_set_config = true;
        if !self.initialized {
            let current = self.device.get_active_configuration().ok();
            need_set_config = current != Some(config);
            self.initialized = true;
        }
        if need_set_config && let Err(e) = self.device.set_active_configuration(config) {
            return Err(Error::SetConfiguration(config, Box::new(e)));
        }

        if let Some(descriptor) = &descriptor {
            self.claim_interfaces(descriptor)?;
            self.rebuild_endpoints(descriptor)?;
        }
        Ok(())
    }

    fn set_interface(&mut self, interface: u8, alt_setting: u8) -> Result<()> {
        self.device
            .set_interface_alt_setting(interface, alt_setting)
            .map_err(|e| Error::SetInterface(interface, alt_setting, Box::new(e)))?;
        self.alt_settings.insert(interface, alt_setting);
        let config = self
            .device
            .get_active_configuration()
            .map_err(|e| Error::GetActiveConfiguration(Box::new(e)))?;
        let descriptor = self.device.get_config_descriptor(config)?;
        self.rebuild_endpoints(&descriptor)?;
        Ok(())
    }

    /// Rebuild the endpoint table from the claimed interfaces of
    /// `config_descriptor` at their current alt settings.
    fn rebuild_endpoints(&mut self, config_descriptor: &ConfigDescriptorTree) -> Result<()> {
        let mut endpoints = HashMap::new();
        for interface in &self.claimed_interfaces {
            let alt_setting = self.alt_settings.get(interface).copied().unwrap_or(0);
            let interface_descriptor = config_descriptor
                .get_interface_descriptor(*interface, alt_setting)
                .ok_or(Error::NoSuchEndpoint(0x80))?;
            for ep_idx in 0..interface_descriptor.bNumEndpoints {
                let Some(ep_dp) = interface_descriptor.get_endpoint_descriptor(ep_idx) else {
                    continue;
                };
                if ep_dp.get_endpoint_number() == 0 {
                    continue;
                }
                endpoints.insert(
                    ep_dp.address(),
                    EndpointInfo {
                        ty: ep_dp
                            .get_endpoint_type()
                            .ok_or_else(|| Error::Other("invalid endpoint type".into()))?,
                        max_packet: ep_dp.max_packet_size(),
                    },
                );
            }
        }
        self.endpoints = endpoints;
        Ok(())
    }

    /// Serve a GET_DESCRIPTOR(Configuration) request from the cached
    /// descriptor tree, rewriting unclaimed interfaces to vendor-specific so
    /// guest drivers leave them alone. Lifted from crosvm's
    /// `get_config_descriptor_filtered`.
    fn config_descriptor_filtered(&self, descriptor_index: u8) -> Result<Vec<u8>> {
        let config_descriptor = self
            .device
            .get_config_descriptor_by_index(descriptor_index)?;
        let device_descriptor = self.device.get_device_descriptor_tree();
        let config_start = config_descriptor.offset();
        let config_end = config_start + config_descriptor.total_length() as usize;
        let mut descriptor_data = device_descriptor.raw()[config_start..config_end].to_vec();

        if self.device.get_active_configuration().ok()
            == Some(config_descriptor.bConfigurationValue)
        {
            for i in 0..config_descriptor.bNumInterfaces {
                if !self.claimed_interfaces.contains(&i) {
                    // Rewrite descriptors for unclaimed interfaces to vendor-specific class.
                    // This prevents them from being recognized by the guest drivers.
                    let alt_setting = self.alt_settings.get(&i).copied().unwrap_or(0);
                    let interface = config_descriptor
                        .get_interface_descriptor(i, alt_setting)
                        .ok_or(Error::NoSuchDescriptor)?;
                    let mut interface_data: types::InterfaceDescriptor = **interface;
                    interface_data.bInterfaceClass = VENDOR_SPECIFIC;
                    interface_data.bInterfaceSubClass = VENDOR_SPECIFIC;
                    interface_data.bInterfaceProtocol = VENDOR_SPECIFIC;

                    let interface_start = interface.offset()
                        + std::mem::size_of::<types::DescriptorHeader>()
                        - config_start;
                    let interface_end =
                        interface_start + std::mem::size_of::<types::InterfaceDescriptor>();
                    descriptor_data[interface_start..interface_end]
                        .copy_from_slice(interface_data.as_bytes());
                }
            }
        }
        Ok(descriptor_data)
    }

    fn intercept_control(&mut self, setup: &SetupPacket) -> Option<UsbTransfer> {
        use ControlRequestDataPhaseTransferDirection as Dir;
        use ControlRequestRecipient as Recipient;
        let direction = if setup.request_type & 0x80 != 0 {
            Dir::DeviceToHost
        } else {
            Dir::HostToDevice
        };
        let recipient = match setup.request_type & 0b1111 {
            0 => Recipient::Device,
            1 => Recipient::Interface,
            2 => Recipient::Endpoint,
            _ => Recipient::Other,
        };
        // Only standard requests are intercepted; class and vendor requests
        // pass through to the device.
        if setup.request_type & 0x60 != 0 {
            return None;
        }
        let request = match setup.request {
            5 => StandardControlRequest::SetAddress,
            6 => StandardControlRequest::GetDescriptor,
            9 => StandardControlRequest::SetConfiguration,
            1 => StandardControlRequest::ClearFeature,
            11 => StandardControlRequest::SetInterface,
            _ => return None,
        };
        match (request, recipient, direction) {
            (StandardControlRequest::SetAddress, Recipient::Device, Dir::HostToDevice) => {
                // usbdevfs owns addressing; nothing to do.
                Some(UsbTransfer::Ack(Vec::new()))
            }
            (StandardControlRequest::SetConfiguration, Recipient::Device, Dir::HostToDevice) => {
                let config = (setup.value & 0xff) as u8;
                Some(match self.set_config(config) {
                    Ok(()) => UsbTransfer::Ack(Vec::new()),
                    Err(e) => {
                        eprintln!("usb host {}: set config {config}: {e}", self.describe());
                        UsbTransfer::Stall
                    }
                })
            }
            (StandardControlRequest::SetInterface, Recipient::Interface, Dir::HostToDevice) => {
                let interface = setup.index as u8;
                let alt = setup.value as u8;
                Some(match self.set_interface(interface, alt) {
                    Ok(()) => UsbTransfer::Ack(Vec::new()),
                    Err(e) => {
                        eprintln!(
                            "usb host {}: set interface {interface} alt {alt}: {e}",
                            self.describe()
                        );
                        UsbTransfer::Stall
                    }
                })
            }
            (StandardControlRequest::ClearFeature, Recipient::Endpoint, Dir::HostToDevice) => {
                if setup.value == STD_FEATURE_ENDPOINT_HALT {
                    Some(match self.device.clear_halt(setup.index as u8) {
                        Ok(()) => UsbTransfer::Ack(Vec::new()),
                        Err(e) => {
                            eprintln!(
                                "usb host {}: clear halt {:#x}: {e}",
                                self.describe(),
                                setup.index
                            );
                            UsbTransfer::Stall
                        }
                    })
                } else {
                    None
                }
            }
            (StandardControlRequest::GetDescriptor, Recipient::Device, Dir::DeviceToHost) => {
                let descriptor_type = (setup.value >> 8) as u8;
                if descriptor_type != types::DescriptorType::Configuration as u8 {
                    return None;
                }
                Some(match self.config_descriptor_filtered(setup.value as u8) {
                    Ok(bytes) => UsbTransfer::Ack(bytes),
                    Err(e) => {
                        eprintln!(
                            "usb host {}: configuration descriptor: {e}",
                            self.describe()
                        );
                        UsbTransfer::Stall
                    }
                })
            }
            _ => None,
        }
    }
}

impl UsbDevice for HostUsbDevice {
    fn speed(&self) -> UsbSpeed {
        self.usb_speed()
    }

    fn reset(&mut self) {
        // The lift source skips USBDEVFS_RESET (it would also drop the
        // configuration and the claims); the xHCI re-initializes slot and
        // endpoint state itself.
        self.device.reset();
    }

    fn control(&mut self, setup: SetupPacket, data: &[u8]) -> UsbTransfer {
        self.reap();
        if let Some(outcome) = self.intercept_control(&setup) {
            return outcome;
        }
        if self.device.is_lost() {
            return UsbTransfer::Stall;
        }
        if matches!(self.pending.get(&0), Some(Pending::Done(_))) {
            return self.take_pending(0);
        }
        if self.pending.contains_key(&0) {
            return UsbTransfer::Nak;
        }
        // The control buffer carries the 8-byte setup packet followed by
        // the data phase: the guest's OUT payload, or receive capacity.
        let mut buffer = Vec::with_capacity(8 + usize::from(setup.length));
        buffer.extend_from_slice(&[
            setup.request_type,
            setup.request,
            setup.value.to_le_bytes()[0],
            setup.value.to_le_bytes()[1],
            setup.index.to_le_bytes()[0],
            setup.index.to_le_bytes()[1],
            setup.length.to_le_bytes()[0],
            setup.length.to_le_bytes()[1],
        ]);
        if setup.request_type & 0x80 != 0 {
            buffer.resize(8 + usize::from(setup.length), 0);
        } else {
            buffer.extend_from_slice(data);
        }
        match Transfer::new_control(buffer) {
            Ok(transfer) => self.start(0, transfer),
            Err(_) => UsbTransfer::Stall,
        }
    }

    fn transfer(&mut self, endpoint: u8, data: &[u8], max_length: usize) -> UsbTransfer {
        self.reap();
        if self.device.is_lost() {
            return UsbTransfer::Stall;
        }
        if matches!(self.pending.get(&endpoint), Some(Pending::Done(_))) {
            return self.take_pending(endpoint);
        }
        if self.pending.contains_key(&endpoint) {
            return UsbTransfer::Nak;
        }
        let Some(info) = self.endpoints.get(&endpoint).copied() else {
            return UsbTransfer::Stall;
        };
        if info.ty == EndpointType::Isochronous {
            return self.transfer_iso(endpoint, data, max_length, info.max_packet as u32);
        }
        let buffer = if endpoint & 0x80 != 0 {
            vec![0; max_length]
        } else {
            data.to_vec()
        };
        let transfer = match info.ty {
            EndpointType::Bulk => Transfer::new_bulk(endpoint, buffer),
            _ => Transfer::new_interrupt(endpoint, buffer),
        };
        match transfer {
            Ok(transfer) => self.start(endpoint, transfer),
            Err(_) => UsbTransfer::Stall,
        }
    }

    fn transfer_iso(
        &mut self,
        endpoint: u8,
        data: &[u8],
        max_length: usize,
        packet_size: u32,
    ) -> UsbTransfer {
        self.reap();
        if self.device.is_lost() {
            return UsbTransfer::Stall;
        }
        if matches!(self.pending.get(&endpoint), Some(Pending::Done(_))) {
            return self.take_pending(endpoint);
        }
        if self.pending.contains_key(&endpoint) {
            return UsbTransfer::Nak;
        }
        let Some(info) = self.endpoints.get(&endpoint).copied() else {
            return UsbTransfer::Stall;
        };
        if info.ty != EndpointType::Isochronous {
            return self.transfer(endpoint, data, max_length);
        }
        // The lift source does not support isochronous OUT passthrough.
        if endpoint & 0x80 == 0 {
            return UsbTransfer::Stall;
        }
        let packet_size = if packet_size == 0 {
            u32::from(info.max_packet)
        } else {
            packet_size
        };
        let buffer = vec![0; max_length];
        match Transfer::new_isochronous(endpoint, buffer, packet_size) {
            Ok(transfer) => self.start(endpoint, transfer),
            Err(_) => UsbTransfer::Stall,
        }
    }

    fn endpoint_dropped(&mut self, endpoint: u8) {
        self.device.discard_endpoint(endpoint);
        self.pending.remove(&endpoint);
        self.discarded.push(endpoint);
    }

    fn is_lost(&self) -> bool {
        self.device.is_lost()
    }

    fn is_host_backed(&self) -> bool {
        true
    }
}

impl Drop for HostUsbDevice {
    fn drop(&mut self) {
        // Give the device back to the host: releasing the claims alone
        // leaves the interfaces driverless, because the claim detached the
        // kernel driver and only USBDEVFS_CONNECT rebinds it.
        self.release_and_reconnect();
    }
}

impl Snapshot for HostUsbDevice {
    fn state_id(&self) -> String {
        format!("usb-host-{}", self.id)
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        Err(DeviceError::State(format!(
            "host USB device {} is attached: online snapshots are refused while it is (D9)",
            self.describe()
        )))
    }

    fn restore_state(&mut self, _bytes: &[u8]) -> Result<(), DeviceError> {
        Err(DeviceError::State(format!(
            "host USB device {} is attached: online snapshots are refused while it is (D9)",
            self.describe()
        )))
    }
}

/// The usbdevfs node for a device number on a bus.
pub fn device_node(bus: u16, dev: u16) -> PathBuf {
    Path::new(USB_DEV_ROOT)
        .join(format!("{bus:03}"))
        .join(format!("{dev:03}"))
}

#[cfg(test)]
mod tests {
    use super::fake::{Completion, FakeUsbFs};
    use super::*;
    use crate::usb::UsbDevice;

    /// A device over the fake, plus the test's handle on the fake.
    fn device() -> (HostUsbDevice, FakeUsbFs) {
        let fake = FakeUsbFs::new();
        let host = HostUsbDevice::new("usbhost-1-2", Box::new(fake.clone())).expect("device");
        (host, fake)
    }

    fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> SetupPacket {
        SetupPacket {
            request_type,
            request,
            value,
            index,
            length,
        }
    }

    /// GET_DESCRIPTOR(Configuration), by descriptor index.
    fn get_config(index: u8) -> SetupPacket {
        setup(0x80, 6, (2 << 8) | u16::from(index), 0, 0xffff)
    }

    fn set_config(value: u8) -> SetupPacket {
        setup(0x00, 9, u16::from(value), 0, 0)
    }

    fn set_interface(index: u8, alt: u8) -> SetupPacket {
        setup(0x01, 11, u16::from(alt), u16::from(index), 0)
    }

    fn clear_halt(endpoint: u8) -> SetupPacket {
        setup(0x02, 1, 0, u16::from(endpoint), 0)
    }

    #[test]
    fn device_node_paths() {
        assert_eq!(device_node(1, 5), Path::new("/dev/bus/usb/001/005"));
        assert_eq!(device_node(12, 321), Path::new("/dev/bus/usb/012/321"));
    }

    #[test]
    fn opening_claims_the_active_configuration() {
        let (host, fake) = device();
        assert_eq!(host.usb_speed(), UsbSpeed::High);
        assert_eq!(host.describe(), "4242:6666");
        // Construction claims both interfaces of configuration 1, which the
        // fake reports as active.
        assert_eq!(fake.calls().claims, vec![0, 1]);
        // Dropping releases the claims and reconnects them: the device
        // returns to the host instead of staying driverless.
        drop(host);
        let calls = fake.calls();
        assert_eq!(calls.releases, vec![0, 1]);
        assert_eq!(calls.connects, vec![0, 1]);
    }

    #[test]
    fn claim_failure_releases_and_reconnects() {
        let fake = FakeUsbFs::new();
        fake.fail_claim(1);
        let error = match HostUsbDevice::new("usbhost-1-2", Box::new(fake.clone())) {
            Ok(_) => panic!("claiming interface 1 must fail"),
            Err(error) => error,
        };
        assert!(matches!(error, Error::ClaimInterface(1, _)), "{error:?}");
        let calls = fake.calls();
        assert_eq!(calls.claims, vec![0]);
        assert_eq!(calls.releases, vec![0]);
        assert_eq!(calls.connects, vec![0]);
    }

    #[test]
    fn intercepted_requests_never_reach_usbdevfs() {
        let (mut host, fake) = device();
        // SET_ADDRESS is a no-op behind usbdevfs.
        assert_eq!(
            host.control(setup(0x00, 5, 0x22, 0, 0), &[]),
            UsbTransfer::Ack(Vec::new())
        );
        // GET_DESCRIPTOR(Configuration) is served from the cached tree.
        let descriptor = host.control(get_config(0), &[]);
        let UsbTransfer::Ack(bytes) = descriptor else {
            panic!("configuration descriptor must be acknowledged");
        };
        assert_eq!(bytes.len(), 0x46);
        assert_eq!(&bytes[..2], &[0x09, 0x02]);
        // Neither interception submits a control URB.
        assert!(fake.submitted_payloads().is_empty());
        assert_eq!(
            host.control(set_config(1), &[]),
            UsbTransfer::Ack(Vec::new())
        );
        assert!(fake.submitted_payloads().is_empty());
    }

    #[test]
    fn set_config_unconfigures_reconfigures_and_reclaims() {
        let (mut host, fake) = device();
        // The first SET_CONFIGURATION(1) matches the active configuration,
        // so no SETCONFIGURATION ioctl is issued, but the claims are retaken.
        assert_eq!(
            host.control(set_config(1), &[]),
            UsbTransfer::Ack(Vec::new())
        );
        assert_eq!(fake.calls().set_configurations, Vec::<u8>::new());
        let after = fake.calls();
        assert_eq!(after.claims, vec![0, 1, 0, 1]);
        assert_eq!(after.releases, vec![0, 1]);

        // Unconfigure: the claims go back and configuration 0 is set.
        assert_eq!(
            host.control(set_config(0), &[]),
            UsbTransfer::Ack(Vec::new())
        );
        let after = fake.calls();
        assert_eq!(after.set_configurations, vec![0]);
        assert_eq!(after.releases, vec![0, 1, 0, 1]);
        assert_eq!(after.claims, vec![0, 1, 0, 1]);

        // Reconfigure: set again and claimed again.
        assert_eq!(
            host.control(set_config(1), &[]),
            UsbTransfer::Ack(Vec::new())
        );
        let after = fake.calls();
        assert_eq!(after.set_configurations, vec![0, 1]);
        assert_eq!(after.claims.len(), 6);
    }

    #[test]
    fn set_interface_switches_alt_setting() {
        let (mut host, fake) = device();
        assert_eq!(
            host.control(set_interface(0, 1), &[]),
            UsbTransfer::Ack(Vec::new())
        );
        assert_eq!(fake.calls().set_interfaces, vec![(0, 1)]);
        // The bulk endpoints survive the alt switch.
        assert_eq!(host.transfer(0x02, b"ping", 64), UsbTransfer::Nak);
    }

    #[test]
    fn clear_feature_halt_clears_the_endpoint() {
        let (mut host, fake) = device();
        assert_eq!(
            host.control(clear_halt(0x81), &[]),
            UsbTransfer::Ack(Vec::new())
        );
        assert_eq!(fake.calls().cleared_halts, vec![0x81]);
    }

    #[test]
    fn bulk_out_naks_then_acknowledges() {
        let (mut host, fake) = device();
        // Submission, then the device's IRQ, then the retry.
        assert_eq!(host.transfer(0x02, b"payload", 64), UsbTransfer::Nak);
        assert_eq!(fake.submitted_payloads(), vec![(0x02, b"payload".to_vec())]);
        fake.run_irq();
        assert_eq!(
            host.transfer(0x02, b"payload", 64),
            UsbTransfer::Ack(Vec::new())
        );
    }

    #[test]
    fn bulk_in_naks_then_delivers_data() {
        let (mut host, fake) = device();
        fake.complete_with(Completion::Data(b"hello".to_vec()));
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        fake.run_irq();
        assert_eq!(
            host.transfer(0x81, &[], 64),
            UsbTransfer::Ack(b"hello".to_vec())
        );
    }

    #[test]
    fn isochronous_in_is_packetized() {
        let (mut host, fake) = device();
        let payload = vec![0xa5u8; 1024];
        fake.complete_with(Completion::Data(payload.clone()));
        assert_eq!(host.transfer_iso(0x83, &[], 1024, 512), UsbTransfer::Nak);
        fake.run_irq();
        assert_eq!(
            host.transfer_iso(0x83, &[], 1024, 512),
            UsbTransfer::Ack(payload)
        );
    }

    #[test]
    fn stalled_completion_halts() {
        let (mut host, fake) = device();
        fake.complete_with(Completion::Stall);
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        fake.run_irq();
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Stall);
    }

    #[test]
    fn unknown_endpoint_stalls() {
        let (mut host, _fake) = device();
        assert_eq!(host.transfer(0x04, b"x", 8), UsbTransfer::Stall);
    }

    #[test]
    fn unplug_marks_the_device_lost() {
        let (mut host, fake) = device();
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        fake.unplug();
        assert!(!host.is_lost());
        // The retry reaps the -ENODEV completions and stalls.
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Stall);
        assert!(host.is_lost());
    }

    #[test]
    fn dropped_endpoint_forgets_in_flight_work() {
        let (mut host, fake) = device();
        // A transfer is in flight when the guest stops the endpoint.
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        // The guest stops the endpoint and re-arms it; the in-flight URB is
        // discarded, so its cancelled completion is never delivered.
        host.endpoint_dropped(0x81);
        fake.run_irq();
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        fake.complete_with(Completion::Data(b"fresh".to_vec()));
        fake.run_irq();
        assert_eq!(
            host.transfer(0x81, &[], 64),
            UsbTransfer::Ack(b"fresh".to_vec())
        );
    }

    #[test]
    fn snapshots_are_refused() {
        let (mut host, _fake) = device();
        assert!(host.save_state().is_err());
        assert!(host.restore_state(&[]).is_err());
    }
}
