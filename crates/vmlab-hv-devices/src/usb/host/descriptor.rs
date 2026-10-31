// Copyright 2019 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE.crosvm file.

//! USB descriptor parsing, lifted from crosvm's usb_util.

use super::error::Result;
use std::collections::BTreeMap;
use std::mem::size_of;
use std::ops::Deref;

use zerocopy::FromBytes;

use super::Error;
use super::types::{
    ConfigDescriptor, Descriptor, DescriptorHeader, DeviceDescriptor, EndpointDescriptor,
    InterfaceDescriptor,
};

#[derive(Clone)]
pub struct DeviceDescriptorTree {
    // Full descriptor tree in the original format returned by the device.
    raw: Vec<u8>,
    inner: DeviceDescriptor,
    // Map of bConfigurationValue to ConfigDescriptor
    config_descriptors: BTreeMap<u8, ConfigDescriptorTree>,
    // Map of config index to bConfigurationValue.
    config_values: BTreeMap<u8, u8>,
}

#[derive(Clone)]
pub struct ConfigDescriptorTree {
    offset: usize,
    inner: ConfigDescriptor,
    // Map of (bInterfaceNumber, bAlternateSetting) to InterfaceDescriptor
    interface_descriptors: BTreeMap<(u8, u8), InterfaceDescriptorTree>,
}

#[derive(Clone)]
pub struct InterfaceDescriptorTree {
    offset: usize,
    inner: InterfaceDescriptor,
    // Map of bEndpointAddress to EndpointDescriptor
    endpoint_descriptors: BTreeMap<u8, EndpointDescriptor>,
}

impl DeviceDescriptorTree {
    pub fn get_config_descriptor(&self, config_value: u8) -> Option<&ConfigDescriptorTree> {
        self.config_descriptors.get(&config_value)
    }

    /// Retrieve the Nth configuration descriptor in the device descriptor.
    /// `config_index`: 0-based index into the list of configuration descriptors.
    pub fn get_config_descriptor_by_index(
        &self,
        config_index: u8,
    ) -> Option<&ConfigDescriptorTree> {
        self.config_descriptors
            .get(self.config_values.get(&config_index)?)
    }

    /// Access the raw descriptor tree as a slice of bytes.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
}

impl Deref for DeviceDescriptorTree {
    type Target = DeviceDescriptor;

    fn deref(&self) -> &DeviceDescriptor {
        &self.inner
    }
}

impl ConfigDescriptorTree {
    /// Get interface by number and alt setting.
    pub fn get_interface_descriptor(
        &self,
        interface_num: u8,
        alt_setting: u8,
    ) -> Option<&InterfaceDescriptorTree> {
        self.interface_descriptors
            .get(&(interface_num, alt_setting))
    }

    /// Get the offset of this configuration descriptor within the raw descriptor tree.
    pub fn offset(&self) -> usize {
        self.offset
    }
}

impl Deref for ConfigDescriptorTree {
    type Target = ConfigDescriptor;

    fn deref(&self) -> &ConfigDescriptor {
        &self.inner
    }
}

impl InterfaceDescriptorTree {
    pub fn get_endpoint_descriptor(&self, ep_idx: u8) -> Option<&EndpointDescriptor> {
        self.endpoint_descriptors.get(&ep_idx)
    }

    /// Get the offset of this interface descriptor within the raw descriptor tree.
    pub fn offset(&self) -> usize {
        self.offset
    }
}

impl Deref for InterfaceDescriptorTree {
    type Target = InterfaceDescriptor;

    fn deref(&self) -> &InterfaceDescriptor {
        &self.inner
    }
}

/// Given `data` containing a full set of descriptors as provided by the Linux kernel
/// usbdevfs `descriptors` file, parse the descriptors into a tree data structure.
pub fn parse_usbfs_descriptors(data: &[u8]) -> Result<DeviceDescriptorTree> {
    let mut offset = 0;

    // Find the next descriptor of type T and return it and its offset.
    // Any other descriptors encountered while searching for the expected type are skipped.
    // The `offset` parameter will be advanced to point to the next byte after the returned
    // descriptor.
    fn next_descriptor<T: Descriptor + FromBytes>(
        data: &[u8],
        offset: &mut usize,
    ) -> Result<(T, usize)> {
        let desc_type = T::descriptor_type() as u8;
        loop {
            let hdr = DescriptorHeader::read_from_bytes(
                data.get(*offset..*offset + size_of::<DescriptorHeader>())
                    .ok_or(Error::DescriptorParse)?,
            )
            .map_err(|_| Error::DescriptorParse)?;
            if hdr.bDescriptorType == desc_type {
                if usize::from(hdr.bLength) < size_of::<DescriptorHeader>() + size_of::<T>() {
                    return Err(Error::DescriptorParse);
                }

                let desc_offset = *offset;

                *offset += size_of::<DescriptorHeader>();
                let desc = T::read_from_bytes(
                    data.get(*offset..*offset + size_of::<T>())
                        .ok_or(Error::DescriptorParse)?,
                )
                .map_err(|_| Error::DescriptorParse)?;
                *offset += hdr.bLength as usize - size_of::<DescriptorHeader>();
                return Ok((desc, desc_offset));
            } else {
                // Finding a ConfigDescriptor while looking for InterfaceDescriptor means
                // that we should advance to the next device configuration.
                if desc_type == InterfaceDescriptor::descriptor_type() as u8
                    && hdr.bDescriptorType == ConfigDescriptor::descriptor_type() as u8
                {
                    return Err(Error::DescriptorParse);
                }

                // Make sure we make forward progress.
                if hdr.bLength == 0 {
                    return Err(Error::DescriptorParse);
                }

                // Skip this entire descriptor, since it's not the right type.
                *offset += hdr.bLength as usize;
            }
        }
    }

    let (raw_device_descriptor, _) = next_descriptor::<DeviceDescriptor>(data, &mut offset)?;
    let mut device_descriptor = DeviceDescriptorTree {
        raw: data.into(),
        inner: raw_device_descriptor,
        config_descriptors: BTreeMap::new(),
        config_values: BTreeMap::new(),
    };

    for cfg_idx in 0..device_descriptor.bNumConfigurations {
        if let Ok((raw_config_descriptor, config_offset)) =
            next_descriptor::<ConfigDescriptor>(&device_descriptor.raw, &mut offset)
        {
            let mut config_descriptor = ConfigDescriptorTree {
                offset: config_offset,
                inner: raw_config_descriptor,
                interface_descriptors: BTreeMap::new(),
            };

            while let Ok((raw_interface_descriptor, interface_offset)) =
                next_descriptor::<InterfaceDescriptor>(&device_descriptor.raw, &mut offset)
            {
                let mut interface_descriptor = InterfaceDescriptorTree {
                    offset: interface_offset,
                    inner: raw_interface_descriptor,
                    endpoint_descriptors: BTreeMap::new(),
                };

                for ep_idx in 0..interface_descriptor.bNumEndpoints {
                    if let Ok((endpoint_descriptor, _)) =
                        next_descriptor::<EndpointDescriptor>(&device_descriptor.raw, &mut offset)
                    {
                        interface_descriptor
                            .endpoint_descriptors
                            .insert(ep_idx, endpoint_descriptor);
                    } else {
                        return Err(Error::DescriptorParse);
                    }
                }

                config_descriptor.interface_descriptors.insert(
                    (
                        interface_descriptor.bInterfaceNumber,
                        interface_descriptor.bAlternateSetting,
                    ),
                    interface_descriptor,
                );
            }

            for intf_idx in 0..config_descriptor.bNumInterfaces {
                if !config_descriptor
                    .interface_descriptors
                    .contains_key(&(intf_idx, 0))
                {
                    return Err(Error::DescriptorParse);
                }
            }

            device_descriptor
                .config_values
                .insert(cfg_idx, config_descriptor.bConfigurationValue);
            device_descriptor
                .config_descriptors
                .insert(config_descriptor.bConfigurationValue, config_descriptor);
        } else {
            return Err(Error::DescriptorParse);
        }
    }

    Ok(device_descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mass-storage descriptors lifted from crosvm's usb_util tests: a USB 3
    // SanDisk reader, one config, one interface, two bulk endpoints.
    const MASS_STORAGE: &[u8] = &[
        0x12, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00, 0x09, 0x81, 0x07, 0x80, 0x55, 0x10, 0x00, 0x01,
        0x02, 0x03, 0x01, 0x09, 0x02, 0x2C, 0x00, 0x01, 0x01, 0x00, 0x80, 0x32, 0x09, 0x04, 0x00,
        0x00, 0x02, 0x08, 0x06, 0x50, 0x00, 0x07, 0x05, 0x81, 0x02, 0x00, 0x04, 0x00, 0x06, 0x30,
        0x0F, 0x00, 0x00, 0x00, 0x07, 0x05, 0x02, 0x02, 0x00, 0x04, 0x00, 0x06, 0x30, 0x0F, 0x00,
        0x00, 0x00,
    ];

    #[test]
    fn parse_descriptors_mass_storage() {
        let d = parse_usbfs_descriptors(MASS_STORAGE).expect("parse_usbfs_descriptors failed");

        // Copy packed fields to locals before the macros borrow them.
        let bcd_usb = d.bcdUSB;
        let id_vendor = d.idVendor;
        let id_product = d.idProduct;
        assert_eq!(bcd_usb, 0x03_00);
        assert_eq!(d.bDeviceClass, 0x00);
        assert_eq!(d.bMaxPacketSize0, 9);
        assert_eq!(id_vendor, 0x0781);
        assert_eq!(id_product, 0x5580);
        assert_eq!(d.bNumConfigurations, 1);

        let c = d
            .get_config_descriptor(1)
            .expect("could not get config descriptor 1");
        let total_length = c.wTotalLength;
        assert_eq!(total_length, 44);
        assert_eq!(c.bNumInterfaces, 1);
        assert_eq!(c.bConfigurationValue, 1);
        assert_eq!(c.bMaxPower, 50);

        let i = c
            .get_interface_descriptor(0, 0)
            .expect("could not get interface descriptor 0 alt setting 0");
        assert_eq!(i.bInterfaceNumber, 0);
        assert_eq!(i.bNumEndpoints, 2);
        assert_eq!(i.bInterfaceClass, 0x08);
        assert_eq!(i.bInterfaceProtocol, 0x50);

        let e = i
            .get_endpoint_descriptor(0)
            .expect("could not get endpoint descriptor 0");
        assert_eq!(e.bEndpointAddress, 0x81);
        assert_eq!(
            e.get_endpoint_type(),
            Some(super::super::types::EndpointType::Bulk)
        );
        assert_eq!(i.get_endpoint_descriptor(1).unwrap().bEndpointAddress, 0x02);
    }

    #[test]
    fn parse_rejects_truncated_input() {
        assert!(parse_usbfs_descriptors(&[]).is_err());
        assert!(parse_usbfs_descriptors(MASS_STORAGE[..20].try_into().unwrap()).is_err());
    }
}
