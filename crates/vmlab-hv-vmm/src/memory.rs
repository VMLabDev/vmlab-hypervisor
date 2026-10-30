//! Guest RAM: anonymous private mmaps, or one shared memfd when the machine
//! has vhost-user-fs shares (virtiofsd maps the same memfd later).

use std::fs::File;
use std::os::fd::FromRawFd;
use std::sync::Arc;

use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use vm_memory::{
    Address, FileOffset, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion,
};

use crate::error::VmmError;
use crate::layout::MemLayout;

/// Built guest memory plus what backs it.
pub struct GuestRam {
    pub mem: GuestMemoryMmap,
    layout: MemLayout,
    /// The memfd backing RAM, when shared; kept open so the mapping lives.
    _file: Option<File>,
    pub(crate) firmware_rom: Option<GuestMemoryMmap>,
}

impl GuestRam {
    /// Anonymous private RAM.
    pub fn anonymous(layout: MemLayout) -> Result<Self, VmmError> {
        let mem = mmap_regions(&layout, None)
            .map_err(|e| VmmError::Memory(format!("allocating guest RAM: {e}")))?;
        Ok(GuestRam {
            mem,
            layout,
            _file: None,
            firmware_rom: None,
        })
    }

    /// RAM backed by one shared memfd, so a vhost-user backend can map the
    /// guest's memory.
    pub fn shared_memfd(layout: MemLayout) -> Result<Self, VmmError> {
        let file = create_memfd().map_err(VmmError::Memory)?;
        let total: u64 = layout
            .regions
            .iter()
            .map(|(base, size)| base + size)
            .max()
            .unwrap();
        // The memfd must cover the high region's offset too; its size is the
        // top of RAM.
        file.set_len(total)
            .map_err(|e| VmmError::Memory(format!("sizing memfd: {e}")))?;
        let mem = mmap_regions(&layout, Some(&file))
            .map_err(|e| VmmError::Memory(format!("mapping guest RAM: {e}")))?;
        Ok(GuestRam {
            mem,
            layout,
            _file: Some(file),
            firmware_rom: None,
        })
    }

    pub fn layout(&self) -> &MemLayout {
        &self.layout
    }
}

fn mmap_regions(
    layout: &MemLayout,
    file: Option<&File>,
) -> Result<GuestMemoryMmap, vm_memory::mmap::FromRangesError> {
    let ranges = layout.regions.iter().map(|(base, size)| {
        let file_offset = file.map(|f| FileOffset::new(f.try_clone().expect("clone memfd"), *base));
        (GuestAddress(*base), *size as usize, file_offset)
    });
    GuestMemoryMmap::from_ranges_with_files(ranges)
}

fn create_memfd() -> Result<File, String> {
    const MFD_CLOEXEC: u32 = 0x0001;
    const MFD_ALLOW_SEALING: u32 = 0x0002;
    // SAFETY: memfd_create(2) with a NUL-terminated name; the returned fd is
    // either a valid file descriptor or -1 with errno set.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"vmlab-hv-ram".as_ptr(),
            MFD_CLOEXEC | MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: fd is a fresh, owned file descriptor from the syscall above.
    Ok(unsafe { File::from_raw_fd(fd as libc::c_int) })
}

/// Register every RAM region as a KVM memslot.
pub fn register_memslots(vm: &Arc<VmFd>, ram: &GuestRam) -> Result<(), VmmError> {
    register_address_space(vm, ram, 0)
}

pub fn register_smm_memslots(vm: &Arc<VmFd>, ram: &GuestRam) -> Result<(), VmmError> {
    register_address_space(vm, ram, 1 << 16)
}

fn register_address_space(
    vm: &Arc<VmFd>,
    ram: &GuestRam,
    address_space: u32,
) -> Result<(), VmmError> {
    for (slot, region) in ram.mem.iter().enumerate() {
        let region = kvm_userspace_memory_region {
            slot: slot as u32 | address_space,
            guest_phys_addr: region.start_addr().raw_value(),
            memory_size: region.size() as u64,
            userspace_addr: region.as_ptr() as u64,
            flags: 0,
        };
        // SAFETY: this is the documented safe wrapper for KVM_SET_USER_MEMORY_REGION.
        unsafe {
            vm.set_user_memory_region(region)
                .map_err(|e| VmmError::ioctl("KVM_SET_USER_MEMORY_REGION", e))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::Bytes;

    #[test]
    fn anonymous_ram_round_trips() {
        let layout = MemLayout::new(8).unwrap();
        let ram = GuestRam::anonymous(layout).unwrap();
        ram.mem.write_slice(b"hello", GuestAddress(0)).unwrap();
        let mut buf = [0u8; 5];
        ram.mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn shared_memfd_ram_round_trips_across_the_hole() {
        let layout = MemLayout::new(4096).unwrap();
        let ram = GuestRam::shared_memfd(layout).unwrap();
        // Write into the high region through a second mapping of the same
        // memfd, as vhost-user would see it.
        let low = &ram.mem;
        low.write_slice(b"low!", GuestAddress(0)).unwrap();
        low.write_slice(b"high", GuestAddress(0x1_0000_0000 + 0x1000))
            .unwrap();
        let mut buf = [0u8; 5];
        low.read_slice(&mut buf, GuestAddress(0x1_0000_0000 + 0x1000))
            .unwrap();
        assert_eq!(&buf[..4], b"high");
        assert_eq!(buf[4], 0);
    }

    #[test]
    fn anonymous_regions_dont_alias() {
        let layout = MemLayout::new(4096).unwrap();
        let ram = GuestRam::anonymous(layout).unwrap();
        ram.mem
            .write_slice(&7u64.to_le_bytes(), GuestAddress(0))
            .unwrap();
        let mut buf = [0u8; 8];
        ram.mem
            .read_slice(&mut buf, GuestAddress(0x1_0000_0000))
            .unwrap();
        assert_eq!(buf, [0; 8]);
    }

    #[test]
    fn memfd_size_covers_high_region() {
        // The memfd length equals the top of RAM, so the high region's file
        // offset (up to 4 GiB in) is inside the file.
        let layout = MemLayout::new(4096).unwrap();
        let ram = GuestRam::shared_memfd(layout).unwrap();
        let f = ram._file.as_ref().unwrap();
        assert_eq!(f.metadata().unwrap().len(), 0x1_4000_0000);
    }
}
