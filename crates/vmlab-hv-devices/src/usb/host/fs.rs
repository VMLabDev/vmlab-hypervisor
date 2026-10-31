// Copyright 2019 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE.crosvm file.

//! The usbdevfs syscall surface.
//!
//! crosvm calls these ioctls directly on the open `File`; here they sit
//! behind a trait so the device layer can be exercised against a fake
//! kernel side (the unit tests) as well as the real one.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_uint, c_void};
use std::ptr;

use super::sys::{
    self, usbdevfs_ctrltransfer, usbdevfs_disconnect_claim, usbdevfs_ioctl, usbdevfs_setinterface,
};

/// The usbdevfs operations a [`super::Device`] needs. Errors carry errno:
/// `WouldBlock` from `reap_urb` means "no completed URB right now" and
/// `NotFound` (ENODEV) means the device is gone.
pub trait UsbFs: Send + Sync {
    /// read(2) the full descriptor blob from the usbdevfs file.
    fn read_descriptors(&self) -> io::Result<Vec<u8>>;
    /// Submit `urb` for asynchronous processing. The URB, and the buffer it
    /// points at, must stay untouched until reaped or discarded.
    fn submit_urb(&self, urb: *mut sys::usbdevfs_urb) -> io::Result<()>;
    /// Reap one completed URB, or `WouldBlock` when none is ready.
    fn reap_urb(&self) -> io::Result<*mut sys::usbdevfs_urb>;
    /// Ask the kernel to cancel an in-flight URB; it is still reaped later,
    /// with a cancelled status.
    fn discard_urb(&self, urb: *mut sys::usbdevfs_urb) -> io::Result<()>;
    /// Synchronous control transfer with a timeout in milliseconds. The
    /// returned value is the number of bytes transferred.
    fn control(&self, transfer: &usbdevfs_ctrltransfer) -> io::Result<c_int>;
    /// Claim `interface`, detaching whatever host kernel driver owns it.
    fn claim_interface(&self, interface: u8) -> io::Result<()>;
    /// Release an interface previously claimed with `claim_interface`.
    fn release_interface(&self, interface: u8) -> io::Result<()>;
    /// Let host kernel drivers bind to `interface` again. This is the
    /// USBDEVFS_CONNECT sub-ioctl; crosvm never issues it (notes/03 F23),
    /// vmlab-hv does, so a detached device returns to the host.
    fn connect_interface(&self, interface: u8) -> io::Result<()>;
    /// Activate an alternate setting for an interface.
    fn set_interface(&self, interface: u8, alt_setting: u8) -> io::Result<()>;
    /// Set the active configuration.
    fn set_configuration(&self, config: u8) -> io::Result<()>;
    /// Clear a halt/stall condition on an endpoint.
    fn clear_halt(&self, endpoint: u8) -> io::Result<()>;
    /// USBDEVFS_GET_SPEED: 1 low, 2 full, 3 high, 4 wireless, 5 super,
    /// 6 super-plus.
    fn get_speed(&self) -> io::Result<c_int>;
}

/// The real usbdevfs: ioctls on an open `/dev/bus/usb/<bus>/<dev>` file.
pub struct Usbdevfs {
    file: File,
}

impl Usbdevfs {
    pub fn new(file: File) -> Self {
        Usbdevfs { file }
    }

    fn ioctl(&self, nr: u32) -> io::Result<c_int> {
        loop {
            // SAFETY: a plain ioctl with no argument on a valid fd; the
            // return value is checked.
            let ret = unsafe { libc::ioctl(self.file.as_raw_fd(), nr as libc::c_ulong) };
            if ret >= 0 {
                return Ok(ret);
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error);
            }
        }
    }

    fn ioctl_with_ptr(&self, nr: u32, arg: *mut c_void) -> io::Result<c_int> {
        loop {
            // SAFETY: the caller guarantees `arg` points at a valid object
            // of the type this ioctl number expects; a pointer is passed
            // pointer-sized through the variadic argument.
            let ret = unsafe { libc::ioctl(self.file.as_raw_fd(), nr as libc::c_ulong, arg) };
            if ret >= 0 {
                return Ok(ret);
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error);
            }
        }
    }

    fn ioctl_with_ref<T>(&self, nr: u32, arg: &T) -> io::Result<c_int> {
        self.ioctl_with_ptr(nr, arg as *const T as *mut c_void)
    }

    fn ioctl_with_mut_ref<T>(&self, nr: u32, arg: &mut T) -> io::Result<c_int> {
        self.ioctl_with_ptr(nr, arg as *mut T as *mut c_void)
    }
}

impl UsbFs for Usbdevfs {
    fn read_descriptors(&self) -> io::Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let mut descriptors = Vec::new();
        file.read_to_end(&mut descriptors)?;
        Ok(descriptors)
    }

    fn submit_urb(&self, urb: *mut sys::usbdevfs_urb) -> io::Result<()> {
        self.ioctl_with_ptr(sys::USBDEVFS_SUBMITURB, urb.cast())
            .map(|_| ())
    }

    fn reap_urb(&self) -> io::Result<*mut sys::usbdevfs_urb> {
        let mut urb_ptr: *mut sys::usbdevfs_urb = ptr::null_mut();
        self.ioctl_with_mut_ref(sys::USBDEVFS_REAPURBNDELAY, &mut urb_ptr)
            .map(|_| urb_ptr)
    }

    fn discard_urb(&self, urb: *mut sys::usbdevfs_urb) -> io::Result<()> {
        self.ioctl_with_ptr(sys::USBDEVFS_DISCARDURB, urb.cast())
            .map(|_| ())
    }

    fn control(&self, transfer: &usbdevfs_ctrltransfer) -> io::Result<c_int> {
        self.ioctl_with_ref(sys::USBDEVFS_CONTROL, transfer)
    }

    fn claim_interface(&self, interface: u8) -> io::Result<()> {
        let disconnect_claim = usbdevfs_disconnect_claim {
            interface: c_uint::from(interface),
            flags: 0,
            driver: [0u8; sys::USBDEVFS_MAXDRIVERNAME + 1],
        };
        self.ioctl_with_ref(sys::USBDEVFS_DISCONNECT_CLAIM, &disconnect_claim)
            .map(|_| ())
    }

    fn release_interface(&self, interface: u8) -> io::Result<()> {
        let ifnum: c_uint = c_uint::from(interface);
        self.ioctl_with_ref(sys::USBDEVFS_RELEASEINTERFACE, &ifnum)
            .map(|_| ())
    }

    fn connect_interface(&self, interface: u8) -> io::Result<()> {
        let connect = usbdevfs_ioctl {
            ifno: c_int::from(interface),
            ioctl_code: sys::USBDEVFS_CONNECT as c_int,
            data: ptr::null_mut(),
        };
        self.ioctl_with_ref(sys::USBDEVFS_IOCTL, &connect)
            .map(|_| ())
    }

    fn set_interface(&self, interface: u8, alt_setting: u8) -> io::Result<()> {
        let setinterface = usbdevfs_setinterface {
            interface: c_uint::from(interface),
            altsetting: c_uint::from(alt_setting),
        };
        self.ioctl_with_ref(sys::USBDEVFS_SETINTERFACE, &setinterface)
            .map(|_| ())
    }

    fn set_configuration(&self, config: u8) -> io::Result<()> {
        let config: c_int = c_int::from(config);
        self.ioctl_with_ref(sys::USBDEVFS_SETCONFIGURATION, &config)
            .map(|_| ())
    }

    fn clear_halt(&self, endpoint: u8) -> io::Result<()> {
        let endpoint: c_uint = c_uint::from(endpoint);
        self.ioctl_with_ref(sys::USBDEVFS_CLEAR_HALT, &endpoint)
            .map(|_| ())
    }

    fn get_speed(&self) -> io::Result<c_int> {
        self.ioctl(sys::USBDEVFS_GET_SPEED)
    }
}
