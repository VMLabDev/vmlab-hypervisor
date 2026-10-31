// Copyright 2019 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE.crosvm file.

//! usbdevfs device access and asynchronous URBs, lifted from crosvm's
//! usb_util `Device`/`Transfer`. Differences from the lift source: the
//! syscall surface sits behind the [`UsbFs`] trait, transfers use vector
//! buffers only (crosvm's mmap DMA buffer is not lifted), completions are
//! reaped by the caller instead of delivered to callbacks, and the
//! `usercontext` URB field is a tag owned by this side rather than the
//! address of an `Arc`.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use super::descriptor::{ConfigDescriptorTree, DeviceDescriptorTree, parse_usbfs_descriptors};
use super::error::{Error, Result};
use super::fs::UsbFs;
use super::sys;
use super::types::{
    DeviceSpeed, StandardControlRequest, control_request_type,
    ControlRequestDataPhaseTransferDirection, ControlRequestRecipient, ControlRequestType,
    UsbRequestSetup,
};

/// A transfer carries one URB. `urb` is a single `Vec` slot holding the URB
/// proper plus zero or more trailing iso packet descriptors, exactly as the
/// kernel ABI expects; `buffer` backs `urb.buffer` and must not be reallocated
/// or moved while the transfer is in flight.
pub struct Transfer {
    urb: Vec<sys::usbdevfs_urb>,
    pub buffer: Vec<u8>,
}

/// One `usbdevfs_urb` with room for `iso_packets` trailing packet descriptors.
fn vec_with_iso_packets(iso_packets: usize) -> Vec<sys::usbdevfs_urb> {
    let iso = std::mem::size_of::<sys::usbdevfs_iso_packet_desc>();
    let bytes =
        std::mem::size_of::<sys::usbdevfs_urb>() + iso_packets * iso;
    let slots = bytes.div_ceil(std::mem::size_of::<sys::usbdevfs_urb>());
    let mut v: Vec<sys::usbdevfs_urb> = Vec::with_capacity(slots);
    // SAFETY: the allocation covers `slots` element-sized slots and the
    // zeroed byte pattern is a valid `usbdevfs_urb` (all-integer/pointer
    // fields; `Default` is implemented by zeroing).
    unsafe {
        std::ptr::write_bytes(v.as_mut_ptr(), 0, slots);
        v.set_len(slots);
    }
    v
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum TransferStatus {
    Completed,
    Error,
    Cancelled,
    NoDevice,
    Stalled,
}

impl Transfer {
    fn urb(&self) -> &sys::usbdevfs_urb {
        &self.urb[0]
    }

    fn urb_mut(&mut self) -> &mut sys::usbdevfs_urb {
        &mut self.urb[0]
    }

    /// The URB address to pass to submit/discard.
    fn urb_ptr(&mut self) -> *mut sys::usbdevfs_urb {
        self.urb.as_mut_ptr()
    }

    fn new(
        transfer_type: u8,
        endpoint: u8,
        buffer: Vec<u8>,
        iso_packets: &[sys::usbdevfs_iso_packet_desc],
    ) -> Result<Transfer> {
        let mut transfer = Transfer {
            urb: vec_with_iso_packets(iso_packets.len()),
            buffer,
        };
        transfer.urb_mut().urb_type = transfer_type;
        transfer.urb_mut().endpoint = endpoint;
        let data = transfer.buffer.as_mut_ptr().cast();
        transfer.urb_mut().buffer = data;
        transfer
            .urb_mut()
            .buffer_length = transfer
            .buffer
            .len()
            .try_into()
            .map_err(Error::InvalidBufferLength)?;

        // SAFETY: `transfer.urb` was created with room for exactly this
        // many trailing iso packet descriptors.
        let iso_frame_desc =
            unsafe { transfer.urb_mut().iso_frame_desc.as_mut_slice(iso_packets.len()) };
        iso_frame_desc.copy_from_slice(iso_packets);

        Ok(transfer)
    }

    /// Create a control transfer. `buffer` is the 8-byte setup packet
    /// followed by the data phase payload (or the receive capacity).
    pub fn new_control(buffer: Vec<u8>) -> Result<Transfer> {
        Self::new(sys::USBDEVFS_URB_TYPE_CONTROL, 0, buffer, &[])
    }

    /// Create an interrupt transfer.
    pub fn new_interrupt(endpoint: u8, buffer: Vec<u8>) -> Result<Transfer> {
        Self::new(sys::USBDEVFS_URB_TYPE_INTERRUPT, endpoint, buffer, &[])
    }

    /// Create a bulk transfer.
    pub fn new_bulk(endpoint: u8, buffer: Vec<u8>) -> Result<Transfer> {
        Self::new(sys::USBDEVFS_URB_TYPE_BULK, endpoint, buffer, &[])
    }

    /// Create an isochronous transfer, dividing `buffer` into packets of
    /// `packet_size` bytes (the last packet takes the remainder).
    pub fn new_isochronous(
        endpoint: u8,
        buffer: Vec<u8>,
        packet_size: u32,
    ) -> Result<Transfer> {
        let buffer_size: u32 = buffer
            .len()
            .try_into()
            .map_err(Error::InvalidBufferLength)?;
        // Isochronous transfers divide the buffer into multiple packets.
        if buffer_size == 0 || packet_size == 0 {
            return Err(Error::InvalidIsochronousParameters);
        }
        let count = buffer_size.div_ceil(packet_size);

        let mut iso_packets = vec![
            sys::usbdevfs_iso_packet_desc {
                length: packet_size,
                actual_length: 0,
                status: 0,
            };
            count as usize
        ];
        let last_entry = iso_packets
            .last_mut()
            .expect("there should be at least one entry for ISOC packet");
        last_entry.length = buffer_size - packet_size * (count - 1);

        let mut transfer = Self::new(
            sys::USBDEVFS_URB_TYPE_ISO,
            endpoint,
            buffer,
            &iso_packets,
        )?;
        transfer.urb_mut().number_of_packets_or_stream_id = count;
        transfer.urb_mut().flags = sys::USBDEVFS_URB_ISO_ASAP;
        Ok(transfer)
    }

    /// Get the status of a completed transfer.
    pub fn status(&self) -> TransferStatus {
        let status = self.urb().status;
        if status == 0 {
            TransferStatus::Completed
        } else if status == -libc::ENODEV || status == -libc::ESHUTDOWN {
            TransferStatus::NoDevice
        } else if status == -libc::ENOENT || status == -libc::ECONNRESET {
            TransferStatus::Cancelled
        } else if status == -libc::EPIPE {
            TransferStatus::Stalled
        } else {
            TransferStatus::Error
        }
    }

    /// Get the actual amount of data transferred, which may be less than
    /// the original length.
    pub fn actual_length(&self) -> usize {
        self.urb().actual_length as usize
    }

    /// The endpoint address the URB carried.
    pub fn endpoint(&self) -> u8 {
        self.urb().endpoint
    }

    /// The transferred data. For control transfers the 8-byte setup prefix
    /// is skipped; for isochronous IN transfers the successful packets'
    /// payloads are concatenated.
    pub fn data(&self) -> &[u8] {
        &self.buffer[..self.actual_length().min(self.buffer.len())]
    }

    /// Control-transfer data phase payload (the setup prefix removed).
    pub fn control_data(&self) -> &[u8] {
        let end = (std::mem::size_of::<UsbRequestSetup>() + self.actual_length())
            .min(self.buffer.len());
        &self.buffer[std::mem::size_of::<UsbRequestSetup>()..end]
    }

    /// Concatenated payloads of the successful iso packets (IN direction).
    pub fn iso_data(&self) -> Vec<u8> {
        // SAFETY: the URB was created with room for exactly
        // `number_of_packets_or_stream_id` trailing descriptors.
        let packets = unsafe {
            self.urb()
                .iso_frame_desc
                .as_slice(self.urb().number_of_packets_or_stream_id as usize)
        };
        let mut data = Vec::new();
        let mut offset = 0usize;
        for packet in packets {
            let length = packet.actual_length as usize;
            if packet.status == 0
                && offset + length <= self.buffer.len()
            {
                data.extend_from_slice(&self.buffer[offset..offset + length]);
            }
            offset += packet.length as usize;
        }
        data
    }
}

/// A USB device behind usbdevfs: descriptors plus asynchronous URBs.
pub struct Device {
    fs: Box<dyn UsbFs>,
    device_descriptor_tree: DeviceDescriptorTree,
    in_flight: HashMap<u64, Transfer>,
    next_tag: u64,
    lost: AtomicBool,
}

impl Device {
    /// Open a device through `fs`, reading and parsing its descriptors.
    pub fn new(fs: Box<dyn UsbFs>) -> Result<Self> {
        let descriptor_data = fs
            .read_descriptors()
            .map_err(Error::DescriptorRead)?;
        let device_descriptor_tree = parse_usbfs_descriptors(&descriptor_data)?;
        Ok(Device {
            fs,
            device_descriptor_tree,
            in_flight: HashMap::new(),
            next_tag: 1,
            lost: AtomicBool::new(false),
        })
    }

    /// Reap completed URBs until the kernel reports none pending. Returns
    /// the completed transfers; sets the lost flag when the device is gone.
    pub fn poll_transfers(&mut self) -> Result<Vec<Transfer>> {
        let mut completed = Vec::new();
        loop {
            match self.fs.reap_urb() {
                Ok(urb_ptr) => {
                    // SAFETY: every submitted URB stays alive in
                    // `in_flight` until reaped here, so the pointer the
                    // kernel hands back points at memory we own.
                    let tag = unsafe { (*urb_ptr).usercontext as u64 };
                    let Some(transfer) = self.in_flight.remove(&tag) else {
                        return Err(Error::Other("reaped an unknown URB".into()));
                    };
                    if matches!(transfer.status(), TransferStatus::NoDevice) {
                        self.lost.store(true, Ordering::SeqCst);
                    }
                    completed.push(transfer);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e)
                    if e.raw_os_error() == Some(libc::ENODEV)
                        || e.raw_os_error() == Some(libc::ESHUTDOWN) =>
                {
                    self.lost.store(true, Ordering::SeqCst);
                    break;
                }
                Err(e) => return Err(Error::IoctlFailed(sys::USBDEVFS_REAPURBNDELAY, e)),
            }
        }
        Ok(completed)
    }

    /// Submit a transfer for asynchronous processing; it comes back through
    /// [`Device::poll_transfers`].
    pub fn submit_transfer(&mut self, mut transfer: Transfer) -> Result<()> {
        if self.is_lost() {
            return Err(Error::NoDevice);
        }
        let tag = self.next_tag;
        self.next_tag += 1;
        transfer.urb_mut().usercontext = tag as usize;
        // SAFETY: the URB lives in `transfer` (or, after a successful
        // submit, in `in_flight`) until it is reaped or discarded.
        let urb_ptr = transfer.urb_ptr();
        if let Err(e) = self.fs.submit_urb(urb_ptr) {
            if e.raw_os_error() == Some(libc::ENODEV) {
                self.lost.store(true, Ordering::SeqCst);
            }
            return Err(Error::IoctlFailed(sys::USBDEVFS_SUBMITURB, e));
        }
        self.in_flight.insert(tag, transfer);
        Ok(())
    }

    /// Discard in-flight transfers on `endpoint`. Discarded URBs are still
    /// reaped later, with a cancelled status.
    pub fn discard_endpoint(&mut self, endpoint: u8) {
        for transfer in self.in_flight.values_mut() {
            if transfer.urb().endpoint == endpoint {
                // SAFETY: the URB lives in `in_flight` until reaped.
                let urb_ptr = std::ptr::addr_of_mut!(transfer.urb[0]);
                let _ = self.fs.discard_urb(urb_ptr);
            }
        }
    }

    /// Whether the device was unplugged or shut down.
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    /// Claim an interface, detaching any host kernel driver bound to it.
    pub fn claim_interface(&self, interface_number: u8) -> Result<()> {
        self.fs.claim_interface(interface_number).map_err(|e| {
            if e.raw_os_error() == Some(libc::ENODEV) {
                self.lost.store(true, Ordering::SeqCst);
            }
            Error::IoctlFailed(sys::USBDEVFS_DISCONNECT_CLAIM, e)
        })
    }

    /// Release an interface previously claimed with `claim_interface`.
    pub fn release_interface(&self, interface_number: u8) -> Result<()> {
        self.fs
            .release_interface(interface_number)
            .map_err(|e| Error::IoctlFailed(sys::USBDEVFS_RELEASEINTERFACE, e))
    }

    /// Ask host kernel drivers to bind to `interface` again
    /// (USBDEVFS_CONNECT). crosvm never issues this; vmlab-hv does so a
    /// detached device returns to the host instead of staying driverless.
    pub fn connect_interface(&self, interface_number: u8) -> Result<()> {
        self.fs
            .connect_interface(interface_number)
            .map_err(|e| Error::IoctlFailed(sys::USBDEVFS_IOCTL, e))
    }

    /// Activate an alternate setting for an interface.
    pub fn set_interface_alt_setting(
        &self,
        interface_number: u8,
        alternative_setting: u8,
    ) -> Result<()> {
        self.fs
            .set_interface(interface_number, alternative_setting)
            .map_err(|e| Error::IoctlFailed(sys::USBDEVFS_SETINTERFACE, e))
    }

    /// Set the active configuration.
    pub fn set_active_configuration(&self, config: u8) -> Result<()> {
        self.fs
            .set_configuration(config)
            .map_err(|e| Error::IoctlFailed(sys::USBDEVFS_SETCONFIGURATION, e))
    }

    /// Get the device descriptor of this device.
    pub fn get_device_descriptor(&self) -> super::types::DeviceDescriptor {
        *self.device_descriptor_tree
    }

    pub fn get_device_descriptor_tree(&self) -> &DeviceDescriptorTree {
        &self.device_descriptor_tree
    }

    /// Get the config descriptor with configuration value `config`.
    pub fn get_config_descriptor(&self, config: u8) -> Result<ConfigDescriptorTree> {
        match self.device_descriptor_tree.get_config_descriptor(config) {
            Some(config_descriptor) => Ok(config_descriptor.clone()),
            None => Err(Error::NoSuchDescriptor),
        }
    }

    /// Get a configuration descriptor by its index within the list of
    /// descriptors returned by the device.
    pub fn get_config_descriptor_by_index(&self, config_index: u8) -> Result<ConfigDescriptorTree> {
        match self
            .device_descriptor_tree
            .get_config_descriptor_by_index(config_index)
        {
            Some(config_descriptor) => Ok(config_descriptor.clone()),
            None => Err(Error::NoSuchDescriptor),
        }
    }

    /// Get bConfigurationValue of the currently active configuration. A
    /// single-configuration device is answered from the descriptors without
    /// a control transfer.
    pub fn get_active_configuration(&self) -> Result<u8> {
        if self.device_descriptor_tree.bNumConfigurations == 1
            && let Some(config_descriptor) = self
                .device_descriptor_tree
                .get_config_descriptor_by_index(0)
        {
            return Ok(config_descriptor.bConfigurationValue);
        }

        // Send a synchronous control transfer to get the active configuration.
        let mut active_config: u8 = 0;
        let ctrl_transfer = sys::usbdevfs_ctrltransfer {
            bRequestType: control_request_type(
                ControlRequestType::Standard,
                ControlRequestDataPhaseTransferDirection::DeviceToHost,
                ControlRequestRecipient::Device,
            ),
            bRequest: StandardControlRequest::GetConfiguration as u8,
            wValue: 0,
            wIndex: 0,
            wLength: std::mem::size_of_val(&active_config) as u16,
            timeout: 5000, // milliseconds
            // SAFETY: a valid one-byte stack slot for the whole call.
            data: std::ptr::addr_of_mut!(active_config).cast(),
        };
        self.fs
            .control(&ctrl_transfer)
            .map_err(|e| Error::IoctlFailed(sys::USBDEVFS_CONTROL, e))?;
        Ok(active_config)
    }

    /// Clear the halt/stall condition for an endpoint.
    pub fn clear_halt(&self, ep_addr: u8) -> Result<()> {
        self.fs
            .clear_halt(ep_addr)
            .map_err(|e| Error::IoctlFailed(sys::USBDEVFS_CLEAR_HALT, e))
    }

    /// Get the speed of this device.
    pub fn get_speed(&self) -> Result<Option<DeviceSpeed>> {
        let speed = self
            .fs
            .get_speed()
            .map_err(|e| Error::IoctlFailed(sys::USBDEVFS_GET_SPEED, e))?;
        Ok(match speed {
            1 => Some(DeviceSpeed::Low),       // Low Speed
            2 => Some(DeviceSpeed::Full),      // Full Speed
            3 => Some(DeviceSpeed::High),      // High Speed
            4 => Some(DeviceSpeed::High),      // Wireless, treat as a High Speed device
            5 => Some(DeviceSpeed::Super),     // SuperSpeed
            6 => Some(DeviceSpeed::SuperPlus), // SuperSpeed Plus
            _ => None,
        })
    }

    /// A port reset through USBDEVFS_RESET is deliberately not issued, as in
    /// the lift source: it also drops the device's configuration and every
    /// interface claim, which the xHCI's port-reset request does not imply.
    pub fn reset(&self) {}
}
