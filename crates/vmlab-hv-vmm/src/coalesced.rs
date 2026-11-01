//! Ordered delivery of KVM's coalesced writes to the legacy VGA aperture.

use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use kvm_bindings::{KVM_COALESCED_MMIO_PAGE_OFFSET, kvm_coalesced_mmio};
use kvm_ioctls::{Cap, IoEventAddress, VcpuFd, VmFd};
use parking_lot::Mutex;

use crate::{VmmError, kvm::KvmVm};

const BASE: u64 = 0xa0000;
const SIZE: u32 = 0x20000;

struct Ring {
    mapping: NonNull<u8>,
    size: usize,
    consumer: Mutex<()>,
}

// SAFETY: the mapping outlives all accesses. Atomic indices synchronize with
// KVM, and consumer serializes every userspace read of the entry array.
unsafe impl Send for Ring {}
// SAFETY: shared access uses atomic indices or the consumer mutex.
unsafe impl Sync for Ring {}

impl Ring {
    fn map(fd: i32, offset: libc::off_t, anonymous: bool) -> Result<Self, VmmError> {
        // SAFETY: sysconf reads a process-independent positive page size.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if size <= 8 + std::mem::size_of::<kvm_coalesced_mmio>() as i64 {
            return Err(VmmError::Memory("invalid coalesced MMIO page size".into()));
        }
        let size = size as usize;
        let flags = libc::MAP_SHARED | if anonymous { libc::MAP_ANONYMOUS } else { 0 };
        // SAFETY: the caller supplies a live vCPU fd at KVM's documented
        // coalesced-ring offset, or requests a fresh anonymous test page.
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                fd,
                offset,
            )
        };
        if mapping == libc::MAP_FAILED {
            return Err(VmmError::Memory(format!(
                "mapping coalesced MMIO: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(Self {
            mapping: NonNull::new(mapping.cast()).expect("mmap returned a null mapping"),
            size,
            consumer: Mutex::new(()),
        })
    }

    fn index(&self, offset: usize) -> &AtomicU32 {
        // SAFETY: both indices are aligned u32 fields at offsets 0 and 4.
        // The page remains mapped, and no non-atomic userspace aliases exist.
        unsafe { &*self.mapping.as_ptr().add(offset).cast::<AtomicU32>() }
    }

    fn pending(&self) -> bool {
        self.index(0).load(Ordering::Acquire) != self.index(4).load(Ordering::Acquire)
    }

    fn drain(&self, mut write: impl FnMut(u64, &[u8])) -> Result<(), VmmError> {
        let _consumer = self.consumer.lock();
        let capacity = (self.size - 8) / std::mem::size_of::<kvm_coalesced_mmio>();
        let mut first = self.index(0).load(Ordering::Relaxed) as usize;
        let end = self.index(4).load(Ordering::Acquire) as usize;
        if first >= capacity || end >= capacity {
            return Err(VmmError::Memory("invalid coalesced MMIO indices".into()));
        }
        while first != end {
            // SAFETY: the acquired producer index publishes this complete
            // entry. KVM cannot reuse it until the release of first below.
            let entry = unsafe {
                self.mapping
                    .as_ptr()
                    .add(8 + first * std::mem::size_of::<kvm_coalesced_mmio>())
                    .cast::<kvm_coalesced_mmio>()
                    .read_volatile()
            };
            let len = entry.len as usize;
            if len == 0
                || len > entry.data.len()
                || entry.phys_addr < BASE
                || entry
                    .phys_addr
                    .checked_add(len as u64)
                    .is_none_or(|end| end > BASE + u64::from(SIZE))
            {
                return Err(VmmError::Memory("invalid coalesced VGA write".into()));
            }
            write(entry.phys_addr - BASE, &entry.data[..len]);
            first = (first + 1) % capacity;
            self.index(0).store(first as u32, Ordering::Release);
        }
        Ok(())
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: this object owns the mapping and all borrowers have ended.
        unsafe { libc::munmap(self.mapping.as_ptr().cast(), self.size) };
    }
}

pub(crate) struct CoalescedVga {
    ring: Ring,
    vm: Arc<VmFd>,
}

impl CoalescedVga {
    pub(crate) fn new(vm: &KvmVm, vcpu: &VcpuFd) -> Result<Option<Self>, VmmError> {
        if !vm.kvm.check_extension(Cap::CoalescedMmio) {
            return Ok(None);
        }
        // SAFETY: sysconf reads the page size used by KVM's mmap ABI.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let ring = Ring::map(
            vcpu.as_raw_fd(),
            i64::from(KVM_COALESCED_MMIO_PAGE_OFFSET) * page_size,
            false,
        )?;
        vm.vm
            .register_coalesced_mmio(IoEventAddress::Mmio(BASE), SIZE)
            .map_err(|e| VmmError::ioctl("registering coalesced VGA writes", e))?;
        Ok(Some(Self {
            ring,
            vm: vm.vm.clone(),
        }))
    }

    pub(crate) fn pending(&self) -> bool {
        self.ring.pending()
    }

    pub(crate) fn drain(&self, write: impl FnMut(u64, &[u8])) -> Result<(), VmmError> {
        self.ring.drain(write)
    }
}

impl Drop for CoalescedVga {
    fn drop(&mut self) {
        if let Err(error) = self
            .vm
            .unregister_coalesced_mmio(IoEventAddress::Mmio(BASE), SIZE)
        {
            eprintln!("unregistering coalesced VGA writes: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(ring: &Ring, index: usize, address: u64, data: &[u8]) {
        let mut entry = kvm_coalesced_mmio {
            phys_addr: address,
            len: data.len() as u32,
            ..Default::default()
        };
        entry.data[..data.len()].copy_from_slice(data);
        // SAFETY: tests own this unpublished ring entry and publish it below.
        unsafe {
            ring.mapping
                .as_ptr()
                .add(8 + index * std::mem::size_of::<kvm_coalesced_mmio>())
                .cast::<kvm_coalesced_mmio>()
                .write_volatile(entry);
        }
    }

    #[test]
    fn entries_wrap_and_writes_finish_before_slots_are_released() {
        let ring = Ring::map(-1, 0, true).unwrap();
        let capacity = (ring.size - 8) / std::mem::size_of::<kvm_coalesced_mmio>();
        ring.index(0)
            .store((capacity - 1) as u32, Ordering::Relaxed);
        push(&ring, capacity - 1, BASE + 12, &[1, 2, 3, 4]);
        push(&ring, 0, BASE + 16, &[5, 6]);
        ring.index(4).store(1, Ordering::Release);
        let mut writes = Vec::new();
        ring.drain(|offset, data| {
            let expected = if writes.is_empty() { capacity - 1 } else { 0 };
            assert_eq!(ring.index(0).load(Ordering::Relaxed) as usize, expected);
            writes.push((offset, data.to_vec()));
        })
        .unwrap();
        assert_eq!(writes, [(12, vec![1, 2, 3, 4]), (16, vec![5, 6])]);
        assert!(!ring.pending());
    }

    #[test]
    fn a_drain_is_bounded_to_entries_published_at_its_start() {
        let ring = Ring::map(-1, 0, true).unwrap();
        push(&ring, 0, BASE, &[1]);
        ring.index(4).store(1, Ordering::Release);
        ring.drain(|_, _| {
            push(&ring, 1, BASE + 1, &[2]);
            ring.index(4).store(2, Ordering::Release);
        })
        .unwrap();
        assert!(ring.pending());
        let mut next = Vec::new();
        ring.drain(|offset, data| next.push((offset, data.to_vec())))
            .unwrap();
        assert_eq!(next, [(1, vec![2])]);
        assert!(!ring.pending());
    }

    #[test]
    fn malformed_entries_do_not_release_or_dispatch_a_slot() {
        for address in [BASE - 1, BASE + u64::from(SIZE), u64::MAX] {
            let ring = Ring::map(-1, 0, true).unwrap();
            push(&ring, 0, address, &[1]);
            ring.index(4).store(1, Ordering::Release);
            assert!(
                ring.drain(|_, _| panic!("invalid write dispatched"))
                    .is_err()
            );
            assert_eq!(ring.index(0).load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn concurrent_consumers_deliver_each_published_write_once_in_order() {
        use std::sync::atomic::AtomicBool;

        let ring = Ring::map(-1, 0, true).unwrap();
        let done = AtomicBool::new(false);
        let received = Mutex::new(Vec::new());
        let capacity = (ring.size - 8) / std::mem::size_of::<kvm_coalesced_mmio>();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for value in 0..10_000u64 {
                    let index = ring.index(4).load(Ordering::Relaxed) as usize;
                    let next = (index + 1) % capacity;
                    while next == ring.index(0).load(Ordering::Acquire) as usize {
                        std::thread::yield_now();
                    }
                    push(&ring, index, BASE, &value.to_le_bytes());
                    ring.index(4).store(next as u32, Ordering::Release);
                }
                done.store(true, Ordering::Release);
            });
            for _ in 0..2 {
                scope.spawn(|| {
                    while !done.load(Ordering::Acquire) || ring.pending() {
                        ring.drain(|_, bytes| {
                            received
                                .lock()
                                .push(u64::from_le_bytes(bytes.try_into().unwrap()));
                        })
                        .unwrap();
                        std::thread::yield_now();
                    }
                });
            }
        });
        assert_eq!(*received.lock(), (0..10_000).collect::<Vec<_>>());
    }
}
