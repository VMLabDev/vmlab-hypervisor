// Copyright 2018 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE.crosvm file.

//! Standard USB descriptor types, lifted from crosvm's usb_util.
#![allow(non_snake_case)]
#![allow(dead_code)]

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Standard USB descriptor types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DescriptorType {
    Device = 1,
    Configuration = 2,
    String = 3,
    Interface = 4,
    Endpoint = 5,
    Qualifier = 6,
    OtherSpeedConfiguration = 7,
    InterfacePower = 8,
    OutputTerminal = 9,
    ExtensionUnit = 11,
}

/// Trait describing USB descriptors.
pub trait Descriptor: IntoBytes + FromBytes {
    /// Return the descriptor's type.
    fn descriptor_type() -> DescriptorType;
}

/// Standard USB descriptor header common to all descriptor types.
#[allow(non_snake_case)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C, packed)]
pub struct DescriptorHeader {
    pub bLength: u8,
    pub bDescriptorType: u8,
}

const _: () = assert!(std::mem::size_of::<DescriptorHeader>() == 2);

/// Standard USB device descriptor as defined in USB 2.0 chapter 9,
/// not including the standard header.
#[allow(non_snake_case)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C, packed)]
pub struct DeviceDescriptor {
    pub bcdUSB: u16,
    pub bDeviceClass: u8,
    pub bDeviceSubClass: u8,
    pub bDeviceProtocol: u8,
    pub bMaxPacketSize0: u8,
    pub idVendor: u16,
    pub idProduct: u16,
    pub bcdDevice: u16,
    pub iManufacturer: u8,
    pub iProduct: u8,
    pub iSerialNumber: u8,
    pub bNumConfigurations: u8,
}

impl Descriptor for DeviceDescriptor {
    fn descriptor_type() -> DescriptorType {
        DescriptorType::Device
    }
}

const _: () = assert!(std::mem::size_of::<DeviceDescriptor>() == 18 - 2);

/// Standard USB configuration descriptor as defined in USB 2.0 chapter 9,
/// not including the standard header.
#[allow(non_snake_case)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C, packed)]
pub struct ConfigDescriptor {
    pub wTotalLength: u16,
    pub bNumInterfaces: u8,
    pub bConfigurationValue: u8,
    pub iConfiguration: u8,
    pub bmAttributes: u8,
    pub bMaxPower: u8,
}

impl Descriptor for ConfigDescriptor {
    fn descriptor_type() -> DescriptorType {
        DescriptorType::Configuration
    }
}

const _: () = assert!(std::mem::size_of::<ConfigDescriptor>() == 9 - 2);

impl ConfigDescriptor {
    pub fn total_length(&self) -> u16 {
        self.wTotalLength
    }
}

/// Standard USB interface descriptor as defined in USB 2.0 chapter 9,
/// not including the standard header.
#[allow(non_snake_case)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C, packed)]
pub struct InterfaceDescriptor {
    pub bInterfaceNumber: u8,
    pub bAlternateSetting: u8,
    pub bNumEndpoints: u8,
    pub bInterfaceClass: u8,
    pub bInterfaceSubClass: u8,
    pub bInterfaceProtocol: u8,
    pub iInterface: u8,
}

impl Descriptor for InterfaceDescriptor {
    fn descriptor_type() -> DescriptorType {
        DescriptorType::Interface
    }
}

const _: () = assert!(std::mem::size_of::<InterfaceDescriptor>() == 9 - 2);

/// Standard USB endpoint descriptor as defined in USB 2.0 chapter 9,
/// not including the standard header.
#[allow(non_snake_case)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C, packed)]
pub struct EndpointDescriptor {
    pub bEndpointAddress: u8,
    pub bmAttributes: u8,
    pub wMaxPacketSize: u16,
    pub bInterval: u8,
}

impl Descriptor for EndpointDescriptor {
    fn descriptor_type() -> DescriptorType {
        DescriptorType::Endpoint
    }
}

const _: () = assert!(std::mem::size_of::<EndpointDescriptor>() == 7 - 2);

const ENDPOINT_DESCRIPTOR_DIRECTION_MASK: u8 = 1 << 7;
const ENDPOINT_DESCRIPTOR_NUMBER_MASK: u8 = 0xf;
const ENDPOINT_DESCRIPTOR_ATTRIBUTES_TYPE_MASK: u8 = 0x3;

/// Endpoint types.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EndpointType {
    Isochronous,
    Bulk,
    Interrupt,
    Control,
}

/// Endpoint Directions.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EndpointDirection {
    HostToDevice,
    DeviceToHost,
}
/// Endpoint direction offset.
pub const ENDPOINT_DIRECTION_OFFSET: u8 = 7;

impl EndpointDescriptor {
    /// Get endpoint number. Returns None if the endpoint number is 0
    /// (not allowed by spec).
    pub fn get_endpoint_number(&self) -> u8 {
        self.bEndpointAddress & ENDPOINT_DESCRIPTOR_NUMBER_MASK
    }
    /// Get endpoint direction.
    pub fn get_direction(&self) -> EndpointDirection {
        if self.bEndpointAddress & ENDPOINT_DESCRIPTOR_DIRECTION_MASK != 0 {
            EndpointDirection::DeviceToHost
        } else {
            EndpointDirection::HostToDevice
        }
    }
    /// Get endpoint type.
    pub fn get_endpoint_type(&self) -> Option<EndpointType> {
        match self.bmAttributes & ENDPOINT_DESCRIPTOR_ATTRIBUTES_TYPE_MASK {
            0 => Some(EndpointType::Control),
            1 => Some(EndpointType::Isochronous),
            2 => Some(EndpointType::Bulk),
            3 => Some(EndpointType::Interrupt),
            _ => None,
        }
    }
    /// The endpoint's maximum packet size, without the additional
    /// transactions-per-microframe bits.
    pub fn max_packet_size(&self) -> u16 {
        self.wMaxPacketSize & 0x7ff
    }
    /// The full endpoint address: number and direction bit.
    pub fn address(&self) -> u8 {
        self.get_endpoint_number()
            | (u8::from(self.get_direction() == EndpointDirection::DeviceToHost)
                << ENDPOINT_DIRECTION_OFFSET)
    }
}

/// Offset of data phase transfer direction.
pub const DATA_PHASE_DIRECTION_OFFSET: u8 = 7;
/// Bit mask of data phase transfer direction.
pub const DATA_PHASE_DIRECTION: u8 = 1u8 << DATA_PHASE_DIRECTION_OFFSET;
/// Types of data phase transfer directions.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ControlRequestDataPhaseTransferDirection {
    HostToDevice,
    DeviceToHost,
}

/// Offset of control request type.
pub const CONTROL_REQUEST_TYPE_OFFSET: u8 = 5;
/// Bit mask of control request type.
pub const CONTROL_REQUEST_TYPE: u8 = 0b11 << CONTROL_REQUEST_TYPE_OFFSET;
/// Request types.
#[derive(Debug, PartialEq, Eq)]
pub enum ControlRequestType {
    Standard,
    Class,
    Vendor,
}

/// Recipient type bits.
pub const REQUEST_RECIPIENT_TYPE: u8 = 0b1111;
/// Recipient type of control request.
#[derive(Debug, PartialEq, Eq)]
pub enum ControlRequestRecipient {
    Device,
    Interface,
    Endpoint,
    Other,
}

/// Standard request defined in usb spec.
#[derive(Debug, PartialEq, Eq)]
pub enum StandardControlRequest {
    GetStatus = 0,
    ClearFeature = 1,
    SetFeature = 3,
    SetAddress = 5,
    GetDescriptor = 6,
    SetDescriptor = 7,
    GetConfiguration = 8,
    SetConfiguration = 9,
    GetInterface = 10,
    SetInterface = 11,
    SynchFrame = 12,
    SetSel = 48,
    SetIsochronousDelay = 49,
}

/// RequestSetup is first part of control transfer buffer.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct UsbRequestSetup {
    pub bmRequestType: u8,
    pub bRequest: u8,
    pub wValue: u16,
    pub wIndex: u16,
    pub wLength: u16,
}

const _: () = assert!(std::mem::size_of::<UsbRequestSetup>() == 8);

impl UsbRequestSetup {
    pub fn new(bmRequestType: u8, bRequest: u8, wValue: u16, wIndex: u16, wLength: u16) -> Self {
        UsbRequestSetup {
            bmRequestType,
            bRequest,
            wValue,
            wIndex,
            wLength,
        }
    }

    /// Get direction of data phase transfer.
    pub fn get_direction(&self) -> ControlRequestDataPhaseTransferDirection {
        if self.bmRequestType & DATA_PHASE_DIRECTION == 0 {
            ControlRequestDataPhaseTransferDirection::HostToDevice
        } else {
            ControlRequestDataPhaseTransferDirection::DeviceToHost
        }
    }

    /// Get recipient type.
    pub fn get_recipient(&self) -> ControlRequestRecipient {
        match self.bmRequestType & REQUEST_RECIPIENT_TYPE {
            0 => ControlRequestRecipient::Device,
            1 => ControlRequestRecipient::Interface,
            2 => ControlRequestRecipient::Endpoint,
            _ => ControlRequestRecipient::Other,
        }
    }

    /// Get control request type. Only standard request type is supported.
    fn get_control_request_type(&self) -> ControlRequestType {
        match self.bmRequestType & CONTROL_REQUEST_TYPE {
            0 => ControlRequestType::Standard,
            1 => ControlRequestType::Class,
            _ => ControlRequestType::Vendor,
        }
    }

    /// Get standard control request.
    pub fn get_standard_request(&self) -> Option<StandardControlRequest> {
        if self.get_control_request_type() != ControlRequestType::Standard {
            return None;
        }
        match self.bRequest {
            0 => Some(StandardControlRequest::GetStatus),
            1 => Some(StandardControlRequest::ClearFeature),
            3 => Some(StandardControlRequest::SetFeature),
            5 => Some(StandardControlRequest::SetAddress),
            6 => Some(StandardControlRequest::GetDescriptor),
            7 => Some(StandardControlRequest::SetDescriptor),
            8 => Some(StandardControlRequest::GetConfiguration),
            9 => Some(StandardControlRequest::SetConfiguration),
            10 => Some(StandardControlRequest::GetInterface),
            11 => Some(StandardControlRequest::SetInterface),
            12 => Some(StandardControlRequest::SynchFrame),
            48 => Some(StandardControlRequest::SetSel),
            49 => Some(StandardControlRequest::SetIsochronousDelay),
            _ => None,
        }
    }
}

/// Construct a bmRequestType value for a control request.
pub fn control_request_type(
    type_: ControlRequestType,
    dir: ControlRequestDataPhaseTransferDirection,
    recipient: ControlRequestRecipient,
) -> u8 {
    let mut bmRequestType: u8 = 0;
    bmRequestType |= match type_ {
        ControlRequestType::Standard => 0,
        ControlRequestType::Class => 1,
        ControlRequestType::Vendor => 2,
    } << CONTROL_REQUEST_TYPE_OFFSET;
    bmRequestType |= match dir {
        ControlRequestDataPhaseTransferDirection::HostToDevice => 0,
        ControlRequestDataPhaseTransferDirection::DeviceToHost => 1,
    } << DATA_PHASE_DIRECTION_OFFSET;
    bmRequestType |= match recipient {
        ControlRequestRecipient::Device => 0,
        ControlRequestRecipient::Interface => 1,
        ControlRequestRecipient::Endpoint => 2,
        ControlRequestRecipient::Other => 3,
    };
    bmRequestType
}

/// USB device speed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceSpeed {
    Full,
    Low,
    High,
    Super,
    SuperPlus,
}

#[cfg(test)]
#[allow(clippy::unusual_byte_groupings)]
mod tests {
    use super::*;

    #[test]
    fn test_control_request_type() {
        assert_eq!(
            control_request_type(
                ControlRequestType::Standard,
                ControlRequestDataPhaseTransferDirection::DeviceToHost,
                ControlRequestRecipient::Device
            ),
            0b1000_0000
        );
        assert_eq!(
            control_request_type(
                ControlRequestType::Standard,
                ControlRequestDataPhaseTransferDirection::HostToDevice,
                ControlRequestRecipient::Interface
            ),
            0b0000_0001
        );
    }

    #[test]
    fn test_standard_request() {
        let setup = UsbRequestSetup::new(0x80, 6, 1, 0, 255);
        assert_eq!(setup.get_standard_request(), Some(StandardControlRequest::GetDescriptor));
        let setup = UsbRequestSetup::new(0x01, 11, 1, 0, 0);
        assert_eq!(setup.get_standard_request(), Some(StandardControlRequest::SetInterface));
        let setup = UsbRequestSetup::new(0x01, 99, 0, 0, 0);
        assert_eq!(setup.get_standard_request(), None);
    }

    #[test]
    fn endpoint_decoding() {
        let mut ep = EndpointDescriptor {
            bEndpointAddress: 0x81,
            bmAttributes: 2,
            wMaxPacketSize: 512,
            bInterval: 0,
        };
        assert_eq!(ep.get_endpoint_number(), 1);
        assert_eq!(ep.get_direction(), EndpointDirection::DeviceToHost);
        assert_eq!(ep.get_endpoint_type(), Some(EndpointType::Bulk));
        assert_eq!(ep.address(), 0x81);
        assert_eq!(ep.max_packet_size(), 512);
        ep.bEndpointAddress = 0x02;
        ep.bmAttributes = 1;
        ep.wMaxPacketSize = 0x1400;
        assert_eq!(ep.get_direction(), EndpointDirection::HostToDevice);
        assert_eq!(ep.get_endpoint_type(), Some(EndpointType::Isochronous));
        assert_eq!(ep.max_packet_size(), 1024);
    }
}
