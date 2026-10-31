// Copyright (C) 2020 Alibaba Cloud. All rights reserved.
//
// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

//! The virtqueue transport for FUSE: a [`Reader`] walks a descriptor chain's
//! device-readable descriptors and a [`Writer`] its writable ones, so the
//! server can decode a request and encode its reply in guest memory.
//!
//! Lifted from fuse-backend-rs 0.14.0 (BSD-3-Clause), cut down to the
//! virtio-fs path over this workspace's vm-memory 0.18 and virtio-queue
//! 0.18. See THIRD_PARTY_NOTICES.

use std::any::Any;
use std::collections::VecDeque;
use std::io::{self, IoSlice, Read, Write};
use std::mem::{MaybeUninit, size_of};
use std::ops::Deref;
use std::ptr::copy_nonoverlapping;
use std::{cmp, fmt};

use libc::{_SC_PAGESIZE, sysconf};
use virtio_queue::DescriptorChain;
use vm_memory::bitmap::{BitmapSlice, MS};
use vm_memory::{
    Address, ByteValued, GuestMemoryBackend, GuestMemoryRegion, MemoryRegionAddress, VolatileSlice,
};

use crate::virtio::fs::fuse::abi::virtio_fs::RemovemappingOne;
use crate::virtio::fs::fuse::file_buf::FileVolatileSlice;
use crate::virtio::fs::fuse::file_traits::FileReadWriteVolatile;

/// Transport layer specific error codes.
#[derive(Debug)]
pub enum Error {
    /// Virtio queue descriptor chain overflows.
    DescriptorChainOverflow,
    /// Failed to find memory region for guest physical address.
    FindMemoryRegion,
    /// Invalid virtio queue descriptor chain.
    InvalidChain,
    /// Invalid paramater.
    InvalidParameter,
    /// Generic IO error.
    IoError(io::Error),
    /// Out of bounds when splitting VolatileSplice.
    SplitOutOfBounds(usize),
    /// Failed to access volatile memory.
    VolatileMemoryError(vm_memory::VolatileMemoryError),
    /// Failed to access guest memory.
    GuestMemoryError(vm_memory::GuestMemoryError),
    /// Invalid Indirect Virtio descriptors.
    ConvertIndirectDescriptor(virtio_queue::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match self {
            DescriptorChainOverflow => write!(
                f,
                "the combined length of all the buffers in a `DescriptorChain` would overflow"
            ),
            FindMemoryRegion => write!(f, "no memory region for this address range"),
            InvalidChain => write!(f, "invalid descriptor chain"),
            InvalidParameter => write!(f, "invalid parameter"),
            IoError(e) => write!(f, "descriptor I/O error: {e}"),
            SplitOutOfBounds(off) => write!(f, "`DescriptorChain` split is out of bounds: {off}"),
            VolatileMemoryError(e) => write!(f, "volatile memory error: {e}"),
            ConvertIndirectDescriptor(e) => write!(f, "invalid indirect descriptor: {e}"),
            GuestMemoryError(e) => write!(f, "descriptor guest memory error: {e}"),
        }
    }
}

impl From<Box<dyn Any + Send>> for Error {
    fn from(value: Box<dyn Any + Send>) -> Self {
        let err = value.downcast::<Error>().unwrap();
        *err
    }
}

/// Specialized version of [std::result::Result] for transport layer operations.
pub type Result<T> = std::result::Result<T, Error>;

impl std::error::Error for Error {}

/// Trait to support virtio-fs DAX Window operations.
///
/// The in-process device does not implement the DAX window, so this stays
/// unimplemented; the server only reaches it for SETUPMAPPING/REMOVEMAPPING,
/// which the guest never sends without a cache capability.
pub trait FsCacheReqHandler: Send + Sync + 'static {
    /// Setup a dedicated mapping so that guest can access file data in DAX style.
    fn map(
        &mut self,
        fov: crate::virtio::fs::fuse::file_buf::FileVolatileSlice,
        offset: u64,
        len: u64,
        req_id: u64,
    ) -> std::io::Result<()>;
    /// Remove a mapping.
    fn unmap(&mut self, requests: Vec<RemovemappingOne>) -> std::io::Result<()>;
}
#[derive(Clone)]
struct IoBuffers<'a, S> {
    buffers: VecDeque<VolatileSlice<'a, S>>,
    bytes_consumed: usize,
}

impl<S: BitmapSlice> Default for IoBuffers<'_, S> {
    fn default() -> Self {
        IoBuffers {
            buffers: VecDeque::new(),
            bytes_consumed: 0,
        }
    }
}

impl<S: BitmapSlice> IoBuffers<'_, S> {
    fn available_bytes(&self) -> usize {
        // This is guaranteed not to overflow because the total length of the chain
        // is checked during all creations of `IoBuffers` (see
        // `Reader::new()` and `Writer::new()`).
        self.buffers
            .iter()
            .fold(0usize, |count, buf| count + buf.len())
    }

    fn bytes_consumed(&self) -> usize {
        self.bytes_consumed
    }

    fn allocate_file_volatile_slice(&self, count: usize) -> Vec<FileVolatileSlice<'_>> {
        let mut rem = count;
        let mut bufs: Vec<FileVolatileSlice> = Vec::with_capacity(self.buffers.len());

        for buf in &self.buffers {
            if rem == 0 {
                break;
            }

            // If buffer contains more data than `rem`, truncate buffer to `rem`, otherwise
            // more data is written out and causes data corruption.
            let local_buf = if buf.len() > rem {
                // Safe because we just check rem < buf.len()
                FileVolatileSlice::from_volatile_slice(&buf.subslice(0, rem).unwrap())
            } else {
                FileVolatileSlice::from_volatile_slice(buf)
            };
            bufs.push(local_buf);

            // Don't need check_sub() as we just made sure rem >= local_buf.len()
            rem -= local_buf.len();
        }

        bufs
    }
    fn mark_dirty(&self, count: usize) {
        let mut rem = count;

        for buf in &self.buffers {
            if rem == 0 {
                break;
            }

            // If buffer contains more data than `rem`, truncate buffer to `rem`, otherwise
            // more data is written out and causes data corruption.
            let local_buf = if buf.len() > rem {
                // Safe because we just check rem < buf.len()
                buf.subslice(0, rem).unwrap()
            } else {
                buf.clone()
            };
            local_buf.bitmap().mark_dirty(0, local_buf.len());

            // Don't need check_sub() as we just made sure rem >= local_buf.len()
            rem -= local_buf.len();
        }
    }

    fn mark_used(&mut self, bytes_consumed: usize) -> io::Result<()> {
        // This can happen if a driver tricks a device into reading/writing more data than
        // fits in a `usize`.
        let total_bytes_consumed =
            self.bytes_consumed
                .checked_add(bytes_consumed)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, Error::DescriptorChainOverflow)
                })?;

        let mut rem = bytes_consumed;
        while let Some(buf) = self.buffers.pop_front() {
            if rem < buf.len() {
                // Split the slice and push the remainder back into the buffer list. Safe because we
                // know that `rem` is not out of bounds due to the check and we checked the bounds
                // on `buf` when we added it to the buffer list.
                self.buffers.push_front(buf.offset(rem).unwrap());
                break;
            }

            // No need for checked math because we know that `buf.size() <= rem`.
            rem -= buf.len();
        }

        self.bytes_consumed = total_bytes_consumed;

        Ok(())
    }

    /// Consumes at most `count` bytes from the `DescriptorChain`. Callers must provide a function
    /// that takes a `&[FileVolatileSlice]` and returns the total number of bytes consumed. This
    /// function guarantees that the combined length of all the slices in the `&[FileVolatileSlice]` is
    /// less than or equal to `count`. `mark_dirty` is used for tracing dirty pages.
    ///
    /// # Errors
    ///
    /// If the provided function returns any error then no bytes are consumed from the buffer and
    /// the error is returned to the caller.
    fn consume<F>(&mut self, mark_dirty: bool, count: usize, f: F) -> io::Result<usize>
    where
        F: FnOnce(&[FileVolatileSlice]) -> io::Result<usize>,
    {
        let bufs = self.allocate_file_volatile_slice(count);
        if bufs.is_empty() {
            Ok(0)
        } else {
            let bytes_consumed = f(&bufs)?;
            if mark_dirty {
                self.mark_dirty(bytes_consumed);
            }
            self.mark_used(bytes_consumed)?;
            Ok(bytes_consumed)
        }
    }

    fn consume_for_read<F>(&mut self, count: usize, f: F) -> io::Result<usize>
    where
        F: FnOnce(&[FileVolatileSlice]) -> io::Result<usize>,
    {
        self.consume(false, count, f)
    }

    fn split_at(&mut self, offset: usize) -> Result<Self> {
        let mut rem = offset;
        let pos = self.buffers.iter().position(|buf| {
            if rem < buf.len() {
                true
            } else {
                rem -= buf.len();
                false
            }
        });

        if let Some(at) = pos {
            let mut other = self.buffers.split_off(at);

            if rem > 0 {
                // There must be at least one element in `other` because we checked
                // its `size` value in the call to `position` above.
                let front = other.pop_front().expect("empty VecDeque after split");
                self.buffers
                    .push_back(front.subslice(0, rem).map_err(Error::VolatileMemoryError)?);
                other.push_front(front.offset(rem).map_err(Error::VolatileMemoryError)?);
            }

            Ok(IoBuffers {
                buffers: other,
                bytes_consumed: 0,
            })
        } else if rem == 0 {
            Ok(IoBuffers {
                buffers: VecDeque::new(),
                bytes_consumed: 0,
            })
        } else {
            Err(Error::SplitOutOfBounds(offset))
        }
    }
}
impl<S: BitmapSlice> IoBuffers<'_, S> {
    /// Consumes for write.
    fn consume_for_write<F>(&mut self, count: usize, f: F) -> io::Result<usize>
    where
        F: FnOnce(&[FileVolatileSlice]) -> io::Result<usize>,
    {
        self.consume(true, count, f)
    }
}
/// Reader to access FUSE requests from the transport layer data buffers.
///
/// Note that virtio spec requires driver to place any device-writable
/// descriptors after any device-readable descriptors (2.6.4.2 in Virtio Spec v1.1).
/// Reader will skip iterating over descriptor chain when first writable
/// descriptor is encountered.
#[derive(Clone)]
pub struct Reader<'a, S = ()> {
    buffers: IoBuffers<'a, S>,
}

impl<S: BitmapSlice> Default for Reader<'_, S> {
    fn default() -> Self {
        Reader {
            buffers: IoBuffers::default(),
        }
    }
}

impl<S: BitmapSlice> Reader<'_, S> {
    /// Reads an object from the descriptor chain buffer.
    pub fn read_obj<T: ByteValued>(&mut self) -> io::Result<T> {
        let mut obj = MaybeUninit::<T>::uninit();

        // Safe because `MaybeUninit` guarantees that the pointer is valid for
        // `size_of::<T>()` bytes.
        let buf = unsafe {
            ::std::slice::from_raw_parts_mut(obj.as_mut_ptr() as *mut u8, size_of::<T>())
        };

        self.read_exact(buf)?;

        // Safe because any type that implements `ByteValued` can be considered initialized
        // even if it is filled with random data.
        Ok(unsafe { obj.assume_init() })
    }

    /// Reads data from the descriptor chain buffer into a file descriptor.
    /// Returns the number of bytes read from the descriptor chain buffer.
    /// The number of bytes read can be less than `count` if there isn't
    /// enough data in the descriptor chain buffer.
    pub fn read_to<F: FileReadWriteVolatile>(
        &mut self,
        mut dst: F,
        count: usize,
    ) -> io::Result<usize> {
        self.buffers
            .consume_for_read(count, |bufs| dst.write_vectored_volatile(bufs))
    }

    /// Reads data from the descriptor chain buffer into a File at offset `off`.
    /// Returns the number of bytes read from the descriptor chain buffer.
    /// The number of bytes read can be less than `count` if there isn't
    /// enough data in the descriptor chain buffer.
    pub fn read_to_at<F: FileReadWriteVolatile>(
        &mut self,
        mut dst: F,
        count: usize,
        off: u64,
    ) -> io::Result<usize> {
        self.buffers
            .consume_for_read(count, |bufs| dst.write_vectored_at_volatile(bufs, off))
    }

    /// Reads exactly size of data from the descriptor chain buffer into a file descriptor.
    pub fn read_exact_to<F: FileReadWriteVolatile>(
        &mut self,
        mut dst: F,
        mut count: usize,
    ) -> io::Result<()> {
        while count > 0 {
            match self.read_to(&mut dst, count) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    ));
                }
                Ok(n) => count -= n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }

    /// Returns number of bytes available for reading.
    ///
    /// May return an error if the combined lengths of all the buffers in the DescriptorChain
    /// would cause an integer overflow.
    pub fn available_bytes(&self) -> usize {
        self.buffers.available_bytes()
    }

    /// Returns number of bytes already read from the descriptor chain buffer.
    pub fn bytes_read(&self) -> usize {
        self.buffers.bytes_consumed()
    }

    /// Splits this `Reader` into two at the given offset in the `DescriptorChain` buffer.
    /// After the split, `self` will be able to read up to `offset` bytes while the returned
    /// `Reader` can read up to `available_bytes() - offset` bytes.  Returns an error if
    /// `offset > self.available_bytes()`.
    pub fn split_at(&mut self, offset: usize) -> Result<Self> {
        self.buffers
            .split_at(offset)
            .map(|buffers| Reader { buffers })
    }
}

impl<S: BitmapSlice> io::Read for Reader<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.buffers.consume_for_read(buf.len(), |bufs| {
            let mut rem = buf;
            let mut total = 0;
            for buf in bufs {
                let copy_len = cmp::min(rem.len(), buf.len());

                // Safe because we have already verified that `buf` points to valid memory.
                unsafe {
                    copy_nonoverlapping(buf.as_ptr() as *const u8, rem.as_mut_ptr(), copy_len);
                }
                rem = &mut rem[copy_len..];
                total += copy_len;
            }
            Ok(total)
        })
    }
}
impl<'a> Reader<'a> {
    /// Construct a new Reader wrapper over `desc_chain`.
    pub fn from_descriptor_chain<M>(
        mem: &'a M::Target,
        desc_chain: DescriptorChain<M>,
    ) -> Result<Reader<'a, MS<'a, M::Target>>>
    where
        M: Deref,
        M::Target: GuestMemoryBackend + Sized,
    {
        let mut total_len: usize = 0;
        // Allocate VecDeque with 64 capacity and hope it could hold all slices to avoid expending
        // VecDeque repeatedly.
        let mut buffers = VecDeque::with_capacity(64);
        for desc in desc_chain.readable() {
            // Verify that summing the descriptor sizes does not overflow.
            // This can happen if a driver tricks a device into reading more data than
            // fits in a `usize`.
            total_len = total_len
                .checked_add(desc.len() as usize)
                .ok_or(Error::DescriptorChainOverflow)?;

            let region = mem
                .find_region(desc.addr())
                .ok_or(Error::FindMemoryRegion)?;
            let offset = desc
                .addr()
                .checked_sub(region.start_addr().raw_value())
                .unwrap();

            buffers.push_back(
                region
                    .get_slice(MemoryRegionAddress(offset.raw_value()), desc.len() as usize)
                    .map_err(Error::GuestMemoryError)?,
            );
        }

        Ok(Reader {
            buffers: IoBuffers {
                buffers,
                bytes_consumed: 0,
            },
        })
    }
}
/// Provide high-level interface over the sequence of memory regions
/// defined by writable descriptors in the Virtio descriptor chain.
///
/// Note that virtio spec requires driver to place any device-writable
/// descriptors after any device-readable descriptors (2.6.4.2 in Virtio Spec v1.1).
/// Writer will start iterating the descriptors from the first writable one and will
/// assume that all following descriptors are writable.
#[derive(Clone)]
pub struct Writer<'a, S = ()> {
    buffers: IoBuffers<'a, S>,
}

impl<'a> Writer<'a> {
    /// Construct a new [Writer] wrapper over `desc_chain`.
    pub fn new<M>(
        mem: &'a M::Target,
        desc_chain: DescriptorChain<M>,
    ) -> Result<Writer<'a, MS<'a, M::Target>>>
    where
        M: Deref,
        M::Target: GuestMemoryBackend + Sized,
    {
        let mut total_len: usize = 0;
        // Allocate VecDeque with 64 capacity and hope it could hold all slices to avoid expending
        // VecDeque repeatedly.
        let mut buffers = VecDeque::with_capacity(64);
        for desc in desc_chain.writable() {
            // Verify that summing the descriptor sizes does not overflow.
            // This can happen if a driver tricks a device into writing more data than
            // fits in a `usize`.
            total_len = total_len
                .checked_add(desc.len() as usize)
                .ok_or(Error::DescriptorChainOverflow)?;

            let region = mem
                .find_region(desc.addr())
                .ok_or(Error::FindMemoryRegion)?;
            let offset = desc
                .addr()
                .checked_sub(region.start_addr().raw_value())
                .unwrap();

            buffers.push_back(
                region
                    .get_slice(MemoryRegionAddress(offset.raw_value()), desc.len() as usize)
                    .map_err(Error::GuestMemoryError)?,
            )
        }

        Ok(Writer {
            buffers: IoBuffers {
                buffers,
                bytes_consumed: 0,
            },
        })
    }
}

impl<'a, S: BitmapSlice> Writer<'a, S> {
    /// Write an object to the descriptor chain buffer.
    pub fn write_obj<T: ByteValued>(&mut self, val: T) -> io::Result<()> {
        self.write_all(val.as_slice())
    }

    /// Write data to the descriptor chain buffer from a file descriptor.
    ///
    /// Return the number of bytes written to the descriptor chain buffer.
    pub fn write_from<F: FileReadWriteVolatile>(
        &mut self,
        mut src: F,
        count: usize,
    ) -> io::Result<usize> {
        self.check_available_space(count, 0, 0)?;
        self.buffers
            .consume_for_write(count, |bufs| src.read_vectored_volatile(bufs))
    }

    /// Write data to the descriptor chain buffer from a File at offset `off`.
    ///
    /// Return the number of bytes written to the descriptor chain buffer.
    pub fn write_from_at<F: FileReadWriteVolatile>(
        &mut self,
        mut src: F,
        count: usize,
        off: u64,
    ) -> io::Result<usize> {
        self.check_available_space(count, 0, 0)?;
        self.buffers
            .consume_for_write(count, |bufs| src.read_vectored_at_volatile(bufs, off))
    }

    /// Write all data to the descriptor chain buffer from a file descriptor.
    pub fn write_all_from<F: FileReadWriteVolatile>(
        &mut self,
        mut src: F,
        mut count: usize,
    ) -> io::Result<()> {
        self.check_available_space(count, 0, 0)?;
        while count > 0 {
            match self.write_from(&mut src, count) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    ));
                }
                Ok(n) => count -= n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }

    /// Return number of bytes available for writing.
    ///
    /// May return an error if the combined lengths of all the buffers in the DescriptorChain would
    /// cause an overflow.
    pub fn available_bytes(&self) -> usize {
        self.buffers.available_bytes()
    }

    /// Return number of bytes already written to the descriptor chain buffer.
    pub fn bytes_written(&self) -> usize {
        self.buffers.bytes_consumed()
    }

    /// Split this `Writer` into two at the given offset in the `DescriptorChain` buffer.
    /// After the split, `self` will be able to write up to `offset` bytes while the returned
    /// `Writer` can write up to `available_bytes() - offset` bytes.  Returns an error if
    /// `offset > self.available_bytes()`.
    pub fn split_at(&mut self, offset: usize) -> Result<Self> {
        self.buffers
            .split_at(offset)
            .map(|buffers| Writer { buffers })
    }

    /// Commit all internal buffers of self and others
    ///
    /// This is provided just to be compatible with fusedev
    pub fn commit(&mut self, _other: Option<&Writer<'a, S>>) -> io::Result<usize> {
        Ok(0)
    }

    fn check_available_space(&self, len1: usize, len2: usize, len3: usize) -> io::Result<()> {
        let len = len1
            .checked_add(len2)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "buffer size is too big"))?;
        let len = len
            .checked_add(len3)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "buffer size is too big"))?;
        if len > self.available_bytes() {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "data out of range, available {} requested {}",
                    self.available_bytes(),
                    len
                ),
            ))
        } else {
            Ok(())
        }
    }
}

impl<S: BitmapSlice> io::Write for Writer<'_, S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.check_available_space(buf.len(), 0, 0)?;

        self.buffers.consume_for_write(buf.len(), |bufs| {
            let mut rem = buf;
            let mut total = 0;
            for buf in bufs {
                let copy_len = cmp::min(rem.len(), buf.len());

                // Safe because we have already verified that `buf` points to valid memory.
                unsafe {
                    copy_nonoverlapping(rem.as_ptr(), buf.as_ptr(), copy_len);
                }
                rem = &rem[copy_len..];
                total += copy_len;
            }
            Ok(total)
        })
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        self.check_available_space(bufs.iter().fold(0, |acc, x| acc + x.len()), 0, 0)?;

        let mut count = 0;
        for buf in bufs.iter().filter(|b| !b.is_empty()) {
            count += self.write(buf)?;
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Nothing to flush since the writes go straight into the buffer.
        Ok(())
    }
}

/// Get system page size.
pub fn pagesize() -> usize {
    // SAFETY: `sysconf(_SC_PAGESIZE)` only reads a constant.
    unsafe { sysconf(_SC_PAGESIZE) as usize }
}
