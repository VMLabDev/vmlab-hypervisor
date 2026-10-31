// Copyright 2019 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE.crosvm file.

//! Linux USB device filesystem (usbdevfs) ioctl bindings.
//!
//! Lifted from crosvm's usb_sys crate (revision
//! ad804a6ecf1d3c4337f18b56092fc54d8ca574fa), which translated it from
//! include/uapi/linux/usbdevice_fs.h. The ioctl numbers are recomputed from
//! the uapi formulas and pinned by tests against the values the kernel
//! header expands to.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]

use std::os::raw::{c_int, c_uchar, c_uint, c_void};

/// `_IOC(dir, type, nr, size)` from asm-generic/ioctl.h.
const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | (ty << 8) | nr
}
const fn io(ty: u32, nr: u32) -> u32 {
    ioc(0, ty, nr, 0)
}
const fn ior<T>(ty: u32, nr: u32) -> u32 {
    ioc(2, ty, nr, std::mem::size_of::<T>() as u32)
}
const fn iow<T>(ty: u32, nr: u32) -> u32 {
    ioc(1, ty, nr, std::mem::size_of::<T>() as u32)
}
const fn iowr<T>(ty: u32, nr: u32) -> u32 {
    ioc(3, ty, nr, std::mem::size_of::<T>() as u32)
}

// Shims for crosvm's base::ioctl_*_nr! generators: each evaluates to the
// ioctl number constant only.
macro_rules! ioctl_io_nr {
    ($name:ident, $ty:ident, $nr:literal) => {
        pub const $name: u32 = io($ty, $nr);
    };
}
macro_rules! ioctl_ior_nr {
    ($name:ident, $ty:ident, $nr:literal, $t:ty) => {
        pub const $name: u32 = ior::<$t>($ty, $nr);
    };
}
macro_rules! ioctl_iow_nr {
    ($name:ident, $ty:ident, $nr:literal, $t:ty) => {
        pub const $name: u32 = iow::<$t>($ty, $nr);
    };
}
macro_rules! ioctl_iowr_nr {
    ($name:ident, $ty:ident, $nr:literal, $t:ty) => {
        pub const $name: u32 = iowr::<$t>($ty, $nr);
    };
}

/// Incomplete array field at the end of a struct, as used by
/// `usbdevfs_urb.iso_frame_desc` and `usbdevfs_streams.eps`.
#[repr(C)]
#[derive(Default)]
pub struct IncompleteArrayField<T>(::std::marker::PhantomData<T>);

impl<T> IncompleteArrayField<T> {
    #[inline]
    pub fn new() -> Self {
        IncompleteArrayField(::std::marker::PhantomData)
    }
    /// # Safety
    ///
    /// Caller must ensure that Self's size and alignment requirements matches
    /// those of `T`s.
    #[inline]
    pub unsafe fn as_ptr(&self) -> *const T {
        self as *const Self as *const T
    }
    /// # Safety
    ///
    /// Caller must ensure that Self's size and alignment requirements matches
    /// those of `T`s.
    #[inline]
    pub unsafe fn as_mut_ptr(&mut self) -> *mut T {
        self as *mut Self as *mut T
    }
    /// # Safety
    ///
    /// Caller must ensure that Self's size and alignment requirements matches
    /// those of `T`s and that `len` elements are in bounds.
    #[inline]
    pub unsafe fn as_slice(&self, len: usize) -> &[T] {
        // SAFETY: callers uphold the layout and bounds contract above.
        unsafe { ::std::slice::from_raw_parts(self.as_ptr(), len) }
    }
    /// # Safety
    ///
    /// Caller must ensure that Self's size and alignment requirements matches
    /// those of `T`s and that `len` elements are in bounds.
    #[inline]
    pub unsafe fn as_mut_slice(&mut self, len: usize) -> &mut [T] {
        // SAFETY: callers uphold the layout and bounds contract above.
        unsafe { ::std::slice::from_raw_parts_mut(self.as_mut_ptr(), len) }
    }
}

impl<T> Clone for IncompleteArrayField<T> {
    #[inline]
    fn clone(&self) -> Self {
        Self::new()
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct usbdevfs_ctrltransfer {
    pub bRequestType: u8,
    pub bRequest: u8,
    pub wValue: u16,
    pub wIndex: u16,
    pub wLength: u16,
    pub timeout: u32,
    pub data: *mut c_void,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct usbdevfs_setinterface {
    pub interface: c_uint,
    pub altsetting: c_uint,
}

pub const USBDEVFS_MAXDRIVERNAME: usize = 255;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct usbdevfs_getdriver {
    pub interface: c_uint,
    pub driver: [u8; USBDEVFS_MAXDRIVERNAME + 1],
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct usbdevfs_connectinfo {
    pub devnum: c_uint,
    pub slow: std::os::raw::c_char,
}

pub const USBDEVFS_URB_SHORT_NOT_OK: c_uint = 0x01;
pub const USBDEVFS_URB_ISO_ASAP: c_uint = 0x02;
pub const USBDEVFS_URB_BULK_CONTINUATION: c_uint = 0x04;
pub const USBDEVFS_URB_NO_FSBR: c_uint = 0x20;
pub const USBDEVFS_URB_ZERO_PACKET: c_uint = 0x40;
pub const USBDEVFS_URB_NO_INTERRUPT: c_uint = 0x80;

pub const USBDEVFS_URB_TYPE_ISO: c_uchar = 0;
pub const USBDEVFS_URB_TYPE_INTERRUPT: c_uchar = 1;
pub const USBDEVFS_URB_TYPE_CONTROL: c_uchar = 2;
pub const USBDEVFS_URB_TYPE_BULK: c_uchar = 3;

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct usbdevfs_iso_packet_desc {
    pub length: c_uint,
    pub actual_length: c_uint,
    pub status: c_uint,
}

#[repr(C)]
#[derive(Clone)]
pub struct usbdevfs_urb {
    pub urb_type: c_uchar,
    pub endpoint: c_uchar,
    pub status: c_int,
    pub flags: c_uint,
    pub buffer: *mut c_void,
    pub buffer_length: c_int,
    pub actual_length: c_int,
    pub start_frame: c_int,
    pub number_of_packets_or_stream_id: c_uint,
    pub error_count: c_int,
    pub signr: c_uint,
    pub usercontext: usize,
    pub iso_frame_desc: IncompleteArrayField<usbdevfs_iso_packet_desc>,
}

impl Default for usbdevfs_urb {
    fn default() -> Self {
        // SAFETY: the struct is all-integer and pointer fields; a zeroed
        // value is valid.
        unsafe { ::std::mem::zeroed() }
    }
}

// SAFETY: the structure that embeds this should ensure that this is safe.
unsafe impl Send for usbdevfs_urb {}
// SAFETY: the structure that embeds this should ensure that this is safe.
unsafe impl Sync for usbdevfs_urb {}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct usbdevfs_ioctl {
    pub ifno: c_int,
    pub ioctl_code: c_int,
    pub data: *mut c_void,
}

pub const USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER: c_uint = 0x01;
pub const USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER: c_uint = 0x02;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct usbdevfs_disconnect_claim {
    pub interface: c_uint,
    pub flags: c_uint,
    pub driver: [u8; USBDEVFS_MAXDRIVERNAME + 1],
}

#[repr(C)]
pub struct usbdevfs_streams {
    pub num_streams: c_uint,
    pub num_eps: c_uint,
    pub eps: IncompleteArrayField<c_uchar>,
}

impl Default for usbdevfs_streams {
    fn default() -> Self {
        // SAFETY: the struct is all-integer fields; a zeroed value is valid.
        unsafe { ::std::mem::zeroed() }
    }
}

const U: u32 = b'U' as u32;

ioctl_iowr_nr!(USBDEVFS_CONTROL, U, 0, usbdevfs_ctrltransfer);
ioctl_ior_nr!(USBDEVFS_SETINTERFACE, U, 4, usbdevfs_setinterface);
ioctl_ior_nr!(USBDEVFS_SETCONFIGURATION, U, 5, c_uint);
ioctl_ior_nr!(USBDEVFS_SUBMITURB, U, 10, usbdevfs_urb);
ioctl_io_nr!(USBDEVFS_DISCARDURB, U, 11);
ioctl_iow_nr!(USBDEVFS_REAPURBNDELAY, U, 13, *mut *mut usbdevfs_urb);
ioctl_ior_nr!(USBDEVFS_CLAIMINTERFACE, U, 15, c_uint);
ioctl_ior_nr!(USBDEVFS_RELEASEINTERFACE, U, 16, c_uint);
ioctl_iowr_nr!(USBDEVFS_IOCTL, U, 18, usbdevfs_ioctl);
ioctl_io_nr!(USBDEVFS_RESET, U, 20);
ioctl_ior_nr!(USBDEVFS_CLEAR_HALT, U, 21, c_uint);
ioctl_io_nr!(USBDEVFS_CONNECT, U, 23);
ioctl_ior_nr!(USBDEVFS_DISCONNECT_CLAIM, U, 27, usbdevfs_disconnect_claim);
ioctl_io_nr!(USBDEVFS_GET_SPEED, U, 31);

#[cfg(test)]
mod tests {
    use super::*;

    /// The uapi header's expansion for this 64-bit ABI, cross-checked
    /// against `gcc -E -dM` on linux/usbdevice_fs.h.
    #[test]
    fn ioctl_numbers_match_the_kernel_header() {
        assert_eq!(USBDEVFS_CONTROL, 0xc018_5500);
        assert_eq!(USBDEVFS_SETINTERFACE, 0x8008_5504);
        assert_eq!(USBDEVFS_SETCONFIGURATION, 0x8004_5505);
        assert_eq!(USBDEVFS_SUBMITURB, 0x8038_550a);
        assert_eq!(USBDEVFS_DISCARDURB, 0x550b);
        assert_eq!(USBDEVFS_REAPURBNDELAY, 0x4008_550d);
        assert_eq!(USBDEVFS_CLAIMINTERFACE, 0x8004_550f);
        assert_eq!(USBDEVFS_RELEASEINTERFACE, 0x8004_5510);
        assert_eq!(USBDEVFS_IOCTL, 0xc010_5512);
        assert_eq!(USBDEVFS_RESET, 0x5514);
        assert_eq!(USBDEVFS_CLEAR_HALT, 0x8004_5515);
        assert_eq!(USBDEVFS_CONNECT, 0x5517);
        assert_eq!(USBDEVFS_DISCONNECT_CLAIM, 0x8108_551b);
        assert_eq!(USBDEVFS_GET_SPEED, 0x551f);
        assert_eq!(std::mem::size_of::<usbdevfs_urb>(), 56);
    }
}
