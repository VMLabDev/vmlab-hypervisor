//! Validated receive spans with optional checksum calculation during DMA copies.

use std::io::{self, Write};

use virtio_queue::DescriptorChain;
use vm_memory::bitmap::Bitmap;
use vm_memory::{GuestMemoryBackend, VolatileSlice};

use crate::GuestMemory;

use super::checksum::CopyChecksum;

pub(super) struct ReceiveWriter<'a> {
    slices: Vec<VolatileSlice<'a>>,
    index: usize,
    offset: usize,
    remaining: usize,
}

impl<'a> ReceiveWriter<'a> {
    pub(super) fn new(
        mem: &'a GuestMemory,
        chain: DescriptorChain<&GuestMemory>,
    ) -> io::Result<Self> {
        let mut remaining = 0usize;
        let slices = chain
            .writable()
            .map(|descriptor| {
                let len = descriptor.len() as usize;
                remaining = remaining.checked_add(len).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "receive chain length overflow")
                })?;
                mem.get_slice(descriptor.addr(), len)
                    .map_err(io::Error::other)
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            slices,
            index: 0,
            offset: 0,
            remaining,
        })
    }

    pub(super) fn available_bytes(&self) -> usize {
        self.remaining
    }

    pub(super) fn write_frame(
        &mut self,
        bytes: &[u8],
        checksum: Option<&mut CopyChecksum>,
    ) -> io::Result<()> {
        self.copy(bytes, checksum)
    }

    pub(super) fn set_flags(&self, flags: u8) -> io::Result<()> {
        let slice = self
            .slices
            .iter()
            .find(|slice| !slice.is_empty())
            .ok_or_else(|| io::Error::new(io::ErrorKind::WriteZero, "missing receive header"))?;
        slice
            .subslice(0, 1)
            .map_err(io::Error::other)?
            .copy_from(&[flags]);
        Ok(())
    }

    fn copy(
        &mut self,
        mut bytes: &[u8],
        mut checksum: Option<&mut CopyChecksum>,
    ) -> io::Result<()> {
        if bytes.len() > self.remaining {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "receive buffer exhausted",
            ));
        }
        while !bytes.is_empty() {
            let original = &self.slices[self.index];
            if self.offset == original.len() {
                self.index += 1;
                self.offset = 0;
                continue;
            }
            let count = (original.len() - self.offset).min(bytes.len());
            let slice = original
                .subslice(self.offset, count)
                .map_err(io::Error::other)?;
            if let Some(checksum) = checksum.as_deref_mut() {
                let guard = slice.ptr_guard_mut();
                // SAFETY: the guarded guest span is writable for count bytes.
                // The host source is disjoint and readable for the same count.
                unsafe { checksum.copy(bytes.as_ptr(), guard.as_ptr(), count) };
                slice.bitmap().mark_dirty(0, count);
            } else {
                slice.copy_from(&bytes[..count]);
            }
            self.offset += count;
            self.remaining -= count;
            bytes = &bytes[count..];
        }
        Ok(())
    }
}

impl Write for ReceiveWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.copy(bytes, None)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
