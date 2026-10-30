//! Direct kernel boot: bzImage, initrd, cmdline, zero page, and the vCPU
//! state the Linux 64-bit entry protocol requires.
//!
//! The bzImage protected-mode part is loaded at 2 MiB with `linux-loader`,
//! the zero page is built at 4 KiB with the e820 map and the RSDP pointer,
//! the initrd sits at the top of low RAM, and the BSP is set up in long mode
//! with identity page tables and a flat GDT before entering
//! `code32_start + 0x200` (the protocol's `startup_64`).

use std::fs::File;
use std::path::Path;

use kvm_ioctls::VcpuFd;
use linux_loader::bootparam::boot_params;
use linux_loader::loader::bzimage::BzImage;
use linux_loader::loader::KernelLoader;
use vm_memory::{ByteValued, Bytes, GuestAddress, GuestMemoryMmap};

use crate::e820::E820Entry;
use crate::error::VmmError;

/// Where the zero page (boot_params) lives.
pub const ZERO_PAGE_ADDR: u64 = 0x1000;
/// Where the NUL-terminated cmdline lives.
pub const CMDLINE_ADDR: u64 = 0x20_000;
pub const CMDLINE_MAX: usize = 0x1000;
/// The protected-mode kernel load address (2 MiB, protocol-aligned).
pub const KERNEL_ADDR: u64 = 0x20_0000;
/// A flat GDT below the zero page.
const GDT_ADDR: u64 = 0x500;
/// Identity page tables: PML4 at 0x9000, one PDPT below it.
const PML4_ADDR: u64 = 0x9000;
const PDPT_ADDR: u64 = 0xA000;

/// Everything the vCPU initializer needs after a load.
#[derive(Debug, Clone, Copy)]
pub struct KernelBoot {
    /// `startup_64`, i.e. the load address + 0x200.
    pub entry: u64,
    /// Address of the zero page (goes in RSI).
    pub zero_page: u64,
}

/// Load a kernel, optional initrd and the cmdline into guest memory.
pub fn load(
    mem: &GuestMemoryMmap,
    kernel: &Path,
    initrd: Option<&Path>,
    cmdline: &str,
    e820: &[E820Entry],
    rsdp_addr: Option<u64>,
    low_ram_top: u64,
) -> Result<KernelBoot, VmmError> {
    let name = kernel.display();
    let mut kernel_file =
        File::open(kernel).map_err(|e| VmmError::Kernel(format!("opening kernel {name}: {e}")))?;

    let loaded = BzImage::load(mem, Some(GuestAddress(KERNEL_ADDR)), &mut kernel_file, None)
        .map_err(|e| VmmError::Kernel(format!("loading bzImage {name}: {e}")))?;
    let mut hdr = loaded
        .setup_header
        .ok_or_else(|| VmmError::Kernel(format!("bzImage {name} has no setup header")))?;

    // Cmdline.
    let cmdline_bytes = cmdline.as_bytes();
    let cmdline_len = cmdline_bytes.len() + 1;
    if cmdline_len > CMDLINE_MAX {
        return Err(VmmError::Kernel(format!(
            "cmdline is {cmdline_len} bytes, limit {CMDLINE_MAX}"
        )));
    }
    mem.write_slice(cmdline_bytes, GuestAddress(CMDLINE_ADDR))
        .map_err(|e| VmmError::Kernel(format!("writing cmdline: {e}")))?;
    mem.write_slice(&[0], GuestAddress(CMDLINE_ADDR + cmdline_bytes.len() as u64))
        .map_err(|e| VmmError::Kernel(format!("writing cmdline: {e}")))?;
    hdr.cmd_line_ptr = CMDLINE_ADDR as u32;
    hdr.cmdline_size = cmdline_len as u32;

    // Initrd: page aligned, at the top of low RAM.
    if let Some(initrd_path) = initrd {
        let bytes = std::fs::read(initrd_path).map_err(|e| {
            VmmError::Kernel(format!("reading initrd {}: {e}", initrd_path.display()))
        })?;
        let addr = (low_ram_top - bytes.len() as u64) & !0xFFF;
        let kernel_end = loaded.kernel_end;
        if addr < kernel_end {
            return Err(VmmError::Kernel(format!(
                "initrd ({} bytes) does not fit below {low_ram_top:#x} after the kernel ends at {kernel_end:#x}",
                bytes.len()
            )));
        }
        mem.write_slice(&bytes, GuestAddress(addr))
            .map_err(|e| VmmError::Kernel(format!("writing initrd: {e}")))?;
        hdr.ramdisk_image = addr as u32;
        hdr.ramdisk_size = bytes.len() as u32;
    }

    // Zero page.
    hdr.type_of_loader = 0xFF; // unknown; no firmware claims this boot
    hdr.boot_flag = 0xAA55;
    let mut params = boot_params::default();
    params.hdr = hdr;
    params.e820_entries = e820.len().min(params.e820_table.len()) as u8;
    for (i, entry) in e820.iter().take(params.e820_table.len()).enumerate() {
        params.e820_table[i] = linux_loader::bootparam::boot_e820_entry {
            addr: entry.addr,
            size: entry.size,
            r#type: entry.kind,
        };
    }
    if let Some(rsdp) = rsdp_addr {
        params.acpi_rsdp_addr = rsdp;
    }
    mem.write_slice(params.as_slice(), GuestAddress(ZERO_PAGE_ADDR))
        .map_err(|e| VmmError::Kernel(format!("writing zero page: {e}")))?;

    setup_long_mode(mem, low_ram_top)?;

    Ok(KernelBoot {
        entry: KERNEL_ADDR + 0x200,
        zero_page: ZERO_PAGE_ADDR,
    })
}

/// Write the identity page tables and GDT the 64-bit entry needs. Maps every
/// 1 GiB page up to `top` (the hole included, which the guest never touches
/// before building its own tables).
fn setup_long_mode(mem: &GuestMemoryMmap, top: u64) -> Result<(), VmmError> {
    const PTE_P: u64 = 0x1;
    const PTE_RW: u64 = 0x2;
    const PTE_PS: u64 = 0x80;

    let gb_pages = top.div_ceil(1 << 30).max(1);
    // One PML4E points at the PDPT, which covers 512 GiB with 1 GiB pages.
    let pml4e = PDPT_ADDR | PTE_P | PTE_RW;
    mem.write_slice(&pml4e.to_le_bytes(), GuestAddress(PML4_ADDR))
        .map_err(|e| VmmError::Kernel(format!("writing PML4: {e}")))?;
    for i in 0..gb_pages {
        let entry = (i << 30) | PTE_PS | PTE_RW | PTE_P;
        mem.write_slice(&entry.to_le_bytes(), GuestAddress(PDPT_ADDR + i * 8))
            .map_err(|e| VmmError::Kernel(format!("writing PDPT: {e}")))?;
    }

    // Flat GDT: null, 64-bit code, data.
    let gdt: [u64; 3] = [0, 0xAF_9B00_0000_FFFF, 0xCF_9300_0000_FFFF];
    for (i, entry) in gdt.iter().enumerate() {
        mem.write_slice(&entry.to_le_bytes(), GuestAddress(GDT_ADDR + i as u64 * 8))
            .map_err(|e| VmmError::Kernel(format!("writing GDT: {e}")))?;
    }
    Ok(())
}

enum SegKind {
    Code64,
    Data,
    Tr,
}

/// Put a freshly created vCPU into the state `startup_64` expects: long mode,
/// identity-mapped, RSI on the zero page.
pub fn init_vcpu(vcpu: &mut VcpuFd, boot: &KernelBoot) -> Result<(), VmmError> {
    let mut regs = vcpu
        .get_regs()
        .map_err(|e| VmmError::ioctl("KVM_GET_REGS", e))?;
    regs.rip = boot.entry;
    regs.rsi = boot.zero_page;
    regs.rflags = 0x2;
    vcpu.set_regs(&regs)
        .map_err(|e| VmmError::ioctl("KVM_SET_REGS", e))?;

    let mut sregs = vcpu
        .get_sregs()
        .map_err(|e| VmmError::ioctl("KVM_GET_SREGS", e))?;
    // PE MP ET NE WP PG
    sregs.cr0 = 0x8001_0033;
    sregs.cr3 = PML4_ADDR;
    sregs.cr4 = 0x20; // PAE
    sregs.efer = 0x500; // LME | LMA
    let seg = |selector: u16, kind: SegKind| -> kvm_bindings::kvm_segment {
        let mut s = kvm_bindings::kvm_segment::default();
        s.selector = selector;
        match kind {
            SegKind::Code64 => {
                s.base = 0;
                s.limit = 0xFFFF_FFFF;
                s.type_ = 11; // execute/read, accessed
                s.present = 1;
                s.s = 1;
                s.l = 1;
                s.g = 1;
            }
            SegKind::Data => {
                s.base = 0;
                s.limit = 0xFFFF_FFFF;
                s.type_ = 3; // read/write, accessed
                s.present = 1;
                s.s = 1;
                s.db = 1;
                s.g = 1;
            }
            SegKind::Tr => {
                s.base = 0;
                s.limit = 0x67;
                s.type_ = 11; // busy 386 TSS
                s.present = 1;
            }
        }
        s
    };
    sregs.cs = seg(0x8, SegKind::Code64);
    let data = seg(0x10, SegKind::Data);
    sregs.ds = data;
    sregs.es = data;
    sregs.ss = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.tr = seg(0x18, SegKind::Tr);
    sregs.ldt.unusable = 1;
    sregs.gdt.base = GDT_ADDR;
    sregs.gdt.limit = 23;
    vcpu.set_sregs(&sregs)
        .map_err(|e| VmmError::ioctl("KVM_SET_SREGS", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::MemLayout;

    const MIB: u64 = 1 << 20;

    // The smallest thing BzImage::load accepts: a setup header with the
    // HdrS magic, a LOADED_HIGH flag, at least 4 setup sectors, and a
    // payload big enough to fill them.
    fn tiny_bzimage() -> Vec<u8> {
        let mut image = vec![0u8; 0x4000];
        image[0x1FE] = 0x55; // boot flag
        image[0x1FF] = 0xAA;
        image[0x202..0x206].copy_from_slice(b"HdrS");
        image[0x206..0x208].copy_from_slice(&0x0208u16.to_le_bytes()); // version
        image[0x211] = 0x01; // LOADED_HIGH
        image[0x1F1] = 4; // setup_sects
        image[0x214..0x218].copy_from_slice(&0x20_0000u32.to_le_bytes()); // code32_start
        image
    }

    fn mem() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 128 * MIB as usize)]).unwrap()
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vmlab-hv-loader-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_kernel(dir: &std::path::Path, image: &[u8]) -> std::path::PathBuf {
        let p = dir.join("vmlinuz");
        std::fs::write(&p, image).unwrap();
        p
    }

    #[test]
    fn loads_kernel_cmdline_and_zero_page() {
        let dir = temp_dir("load");
        let kernel = write_kernel(&dir, &tiny_bzimage());
        let mem = self::mem();
        let layout = MemLayout::new(128).unwrap();
        let boot = load(
            &mem,
            &kernel,
            None,
            "console=ttyS0 quiet",
            &crate::e820::build_e820(&layout),
            Some(0x9_E000),
            layout.low_ram_top(),
        )
        .unwrap();
        assert_eq!(boot.entry, KERNEL_ADDR + 0x200);
        assert_eq!(boot.zero_page, ZERO_PAGE_ADDR);

        // The zero page keeps the header magic and points at the cmdline.
        let params: boot_params = mem.read_obj(GuestAddress(ZERO_PAGE_ADDR)).unwrap();
        let hdr_magic = { params.hdr.header };
        assert_eq!(hdr_magic, 0x5372_6448); // "HdrS"
        let cmd_line_ptr = { params.hdr.cmd_line_ptr };
        assert_eq!(cmd_line_ptr, CMDLINE_ADDR as u32);
        let boot_flag = { params.hdr.boot_flag };
        assert_eq!(boot_flag, 0xAA55);

        // Cmdline bytes plus NUL.
        let mut buf = [0u8; 20];
        mem.read_slice(&mut buf, GuestAddress(CMDLINE_ADDR)).unwrap();
        assert_eq!(&buf, b"console=ttyS0 quiet\0");

        // e820 count and the RSDP pointer.
        let e820_entries = { params.e820_entries };
        assert_eq!(e820_entries as usize, crate::e820::build_e820(&layout).len());
        let rsdp = { params.acpi_rsdp_addr };
        assert_eq!(rsdp, 0x9_E000);

        // Identity page tables and GDT present.
        let mut pml4e = [0u8; 8];
        mem.read_slice(&mut pml4e, GuestAddress(PML4_ADDR)).unwrap();
        assert_eq!(u64::from_le_bytes(pml4e) & 0xF, 0x3);
        let mut gdt = [0u8; 8];
        mem.read_slice(&mut gdt, GuestAddress(GDT_ADDR + 8)).unwrap();
        assert_eq!(u64::from_le_bytes(gdt), 0xAF_9B00_0000_FFFF);
    }

    #[test]
    fn initrd_lands_below_low_ram_top() {
        let dir = temp_dir("initrd");
        let kernel = write_kernel(&dir, &tiny_bzimage());
        let initrd = dir.join("initrd");
        std::fs::write(&initrd, vec![7u8; 0x2100]).unwrap();
        let mem = self::mem();
        let layout = MemLayout::new(128).unwrap();
        load(
            &mem,
            &kernel,
            Some(&initrd),
            "",
            &crate::e820::build_e820(&layout),
            None,
            layout.low_ram_top(),
        )
        .unwrap();
        let params: boot_params = mem.read_obj(GuestAddress(ZERO_PAGE_ADDR)).unwrap();
        let ramdisk_image = { params.hdr.ramdisk_image };
        let addr = ramdisk_image as u64;
        assert_eq!(addr % 0x1000, 0);
        assert!(addr + 0x2100 <= layout.low_ram_top());
        let mut buf = [0u8; 3];
        mem.read_slice(&mut buf, GuestAddress(addr)).unwrap();
        assert_eq!(buf, [7, 7, 7]);
    }

    #[test]
    fn missing_kernel_is_a_clear_error() {
        let mem = self::mem();
        let err = load(
            &mem,
            Path::new("/nonexistent/vmlinuz"),
            None,
            "",
            &[],
            None,
            128 * MIB,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/nonexistent/vmlinuz"), "{msg}");
    }

    #[test]
    fn oversized_cmdline_is_rejected() {
        let dir = temp_dir("cmdline");
        let kernel = write_kernel(&dir, &tiny_bzimage());
        let mem = self::mem();
        let err = load(
            &mem,
            &kernel,
            None,
            &"a".repeat(CMDLINE_MAX),
            &[],
            None,
            128 * MIB,
        )
        .unwrap_err();
        assert!(err.to_string().contains("cmdline"), "{err}");
    }
}
