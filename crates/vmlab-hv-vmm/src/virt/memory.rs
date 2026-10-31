use parking_lot::{Mutex, ReentrantMutex};
use std::sync::Arc;
use vm_memory::{Bytes, GuestAddress};
use vmlab_hv_devices::GuestMemory;
use vmlab_hv_tcg::{CodeStamp, Error, Memory, Result};

use super::RAM_BASE;
use crate::{bus::MmioBus, pci::PciRoot};

#[derive(Clone)]
pub struct PhysicalMemory {
    pub ram: GuestMemory,
    pub size: u64,
    pub bus: Arc<Mutex<MmioBus>>,
    pub pci: Arc<Mutex<PciRoot>>,
    transaction: Arc<ReentrantMutex<()>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
    pages: Arc<Vec<std::sync::atomic::AtomicU64>>,
    identity: u64,
    cache_safe: Arc<std::sync::atomic::AtomicBool>,
    flash: Vec<(u64, Arc<Mutex<super::flash::Flash>>)>,
}

impl PhysicalMemory {
    pub fn new(ram: GuestMemory, size: u64, bus: MmioBus, pci: Arc<Mutex<PciRoot>>) -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            ram,
            size,
            bus: Arc::new(Mutex::new(bus)),
            pci,
            transaction: Arc::new(ReentrantMutex::new(())),
            generation: Arc::default(),
            pages: Arc::new(
                (0..size.div_ceil(4096))
                    .map(|_| std::sync::atomic::AtomicU64::new(0))
                    .collect(),
            ),
            identity: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            cache_safe: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            flash: Vec::new(),
        }
    }

    pub(super) fn map_flash(&mut self, base: u64, flash: Arc<Mutex<super::flash::Flash>>) {
        self.flash.push((base, flash));
    }

    fn is_ram(&self, address: u64, len: usize) -> bool {
        address >= RAM_BASE
            && address
                .checked_add(len as u64)
                .is_some_and(|end| end <= RAM_BASE + self.size)
    }
}

impl Memory for PhysicalMemory {
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()> {
        let _guard = self.transaction.lock();
        if self.is_ram(address, bytes.len()) {
            self.ram
                .read_slice(bytes, GuestAddress(address))
                .map_err(|_| Error::Memory(address))?;
        } else if (0x300_0000..0x301_0000).contains(&address) {
            self.pci
                .lock()
                .dispatch_io_read((address - 0x300_0000) as u16, bytes);
        } else if !self.pci.lock().dispatch_mmio_read(address, bytes) {
            self.bus.lock().read(address, bytes);
        }
        Ok(())
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        let _guard = self.transaction.lock();
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .wrapping_add(1);
        // These platform registers cannot initiate DMA. Any other MMIO write
        // permanently revokes RAM code stamps before firmware can start a PCI
        // device or fw_cfg DMA. Device writes bypass this CPU memory adapter.
        if !self.is_ram(address, bytes.len())
            && !matches!(address,
            0x100000..=0x101fff | 0x2000000..=0x200ffff | 0xc000000..=0xc5fffff | 0x10000000..=0x100000ff | 0x20000000..=0x23ffffff)
        {
            self.cache_safe
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        if self.is_ram(address, bytes.len()) {
            self.ram
                .write_slice(bytes, GuestAddress(address))
                .map_err(|_| Error::Memory(address))?;
            if !bytes.is_empty() {
                let first = ((address - RAM_BASE) / 4096) as usize;
                let last = ((address - RAM_BASE + bytes.len() as u64 - 1) / 4096) as usize;
                for page in &self.pages[first..=last] {
                    page.store(generation, std::sync::atomic::Ordering::SeqCst);
                }
            }
        } else if (0x300_0000..0x301_0000).contains(&address) {
            self.pci
                .lock()
                .dispatch_io_write((address - 0x300_0000) as u16, bytes);
        } else if !self.pci.lock().dispatch_mmio_write(address, bytes) {
            self.bus.lock().write(address, bytes);
        }
        Ok(())
    }
    fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn ram_bounds(&self, address: u64) -> Option<(u64, u64)> {
        self.is_ram(address, 1)
            .then_some((RAM_BASE, RAM_BASE + self.size - 1))
    }
    fn code_stamp(&self, address: u64, len: usize) -> Option<CodeStamp> {
        let _guard = self.transaction.lock();
        if len == 0 {
            return None;
        }
        for (base, flash) in &self.flash {
            if address >= *base
                && address
                    .checked_add(len as u64)
                    .is_some_and(|end| end <= base + (32 << 20))
            {
                return Some(CodeStamp {
                    identity: self.identity,
                    mapping: *base,
                    first_page: (address - base) / 4096,
                    last_page: (address - base + len as u64 - 1) / 4096,
                    generation: flash.lock().code_generation()?,
                });
            }
        }
        if !self.cache_safe.load(std::sync::atomic::Ordering::SeqCst) || !self.is_ram(address, len)
        {
            return None;
        }
        let first = (address - RAM_BASE) / 4096;
        let last = (address - RAM_BASE + len as u64 - 1) / 4096;
        Some(CodeStamp {
            identity: self.identity,
            mapping: RAM_BASE,
            first_page: first,
            last_page: last,
            generation: self.pages[first as usize..=last as usize]
                .iter()
                .map(|p| p.load(std::sync::atomic::Ordering::SeqCst))
                .max()?,
        })
    }
    fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> Result<u64> {
        let transaction = self.transaction.clone();
        let _guard = transaction.lock();
        self.read(address, bytes)?;
        Ok(self.generation())
    }
    fn store_conditional(&mut self, address: u64, bytes: &[u8], generation: u64) -> Result<bool> {
        let transaction = self.transaction.clone();
        let _guard = transaction.lock();
        if self.generation() != generation {
            return Ok(false);
        }
        self.write(address, bytes)?;
        Ok(true)
    }
    fn transaction_lock(&self) -> Option<Arc<ReentrantMutex<()>>> {
        Some(self.transaction.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> PhysicalMemory {
        let size = 4 * 4096;
        let ram = GuestMemory::from_ranges(&[(GuestAddress(RAM_BASE), size)]).unwrap();
        PhysicalMemory::new(
            ram,
            size as u64,
            MmioBus::new(),
            Arc::new(Mutex::new(PciRoot::q35(0x4_0000_0000))),
        )
    }

    #[test]
    fn flash_code_stamps_follow_command_mode_programming_and_restore() {
        use vmlab_hv_devices::{BusDevice, Snapshot};
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(32 << 20).unwrap();
        let flash = Arc::new(Mutex::new(
            super::super::flash::Flash::open("flash", file.path(), true).unwrap(),
        ));
        let mut memory = memory();
        memory
            .bus
            .lock()
            .register(0x20000000, 32 << 20, flash.clone())
            .unwrap();
        memory.map_flash(0x20000000, flash.clone());
        let initial = memory.code_stamp(0x20000000, 4).unwrap();
        let state = flash.lock().save_state().unwrap();
        memory.write(0x20000000, &[0x98]).unwrap();
        assert!(memory.code_stamp(0x20000000, 4).is_none());
        flash.lock().write(0, &[0xff]);
        assert_ne!(memory.code_stamp(0x20000000, 4), Some(initial));
        let array = memory.code_stamp(0x20000000, 4).unwrap();
        flash.lock().restore_state(&state).unwrap();
        assert_ne!(memory.code_stamp(0x20000000, 4), Some(array));
        memory.write(0x30008004, &7u16.to_le_bytes()).unwrap();
        assert!(memory.code_stamp(RAM_BASE, 4).is_none());
        assert!(memory.code_stamp(0x20000000, 4).is_some());
        assert!(memory.code_stamp(0x21ffffff, 2).is_none());
    }

    #[test]
    fn shared_code_stamps_invalidate_only_written_pages() {
        let mut first = memory();
        let mut second = first.clone();
        let code = first.code_stamp(RAM_BASE, 4).unwrap();
        let crossing = first.code_stamp(RAM_BASE + 4094, 4).unwrap();
        second.write(RAM_BASE + 8192, &[1]).unwrap();
        assert_eq!(first.code_stamp(RAM_BASE, 4), Some(code));
        assert_eq!(first.code_stamp(RAM_BASE + 4094, 4), Some(crossing));
        first.write(RAM_BASE + 4096, &[1]).unwrap();
        assert_ne!(second.code_stamp(RAM_BASE + 4094, 4), Some(crossing));
        assert_eq!(second.code_stamp(RAM_BASE, 4), Some(code));
        assert_ne!(memory().code_stamp(RAM_BASE, 4), Some(code));
        assert!(first.code_stamp(RAM_BASE - 1, 4).is_none());
        assert!(first.code_stamp(RAM_BASE + first.size - 1, 4).is_none());
    }

    #[test]
    fn possible_dma_revokes_code_stamps_before_device_access() {
        let mut memory = memory();
        let other = memory.clone();
        memory.write(0x2000000, &1u32.to_le_bytes()).unwrap();
        memory.write(0x10000000, &[65]).unwrap();
        assert!(other.code_stamp(RAM_BASE, 4).is_some());
        memory.write(0x30008004, &7u16.to_le_bytes()).unwrap();
        assert!(other.code_stamp(RAM_BASE, 4).is_none());
        memory.write(RAM_BASE, &[1]).unwrap();
        assert!(other.code_stamp(RAM_BASE, 4).is_none());
    }

    #[test]
    fn shared_reservations_fail_after_another_hart_store() {
        let mut first = memory();
        let mut second = first.clone();
        let mut word = [0; 8];
        let generation = first.load_reserved(RAM_BASE, &mut word).unwrap();
        second.write(RAM_BASE, &5u64.to_le_bytes()).unwrap();
        assert!(
            !first
                .store_conditional(RAM_BASE, &7u64.to_le_bytes(), generation)
                .unwrap()
        );
        let generation = first.load_reserved(RAM_BASE, &mut word).unwrap();
        assert!(
            first
                .store_conditional(RAM_BASE, &9u64.to_le_bytes(), generation)
                .unwrap()
        );
        second.read(RAM_BASE, &mut word).unwrap();
        assert_eq!(u64::from_le_bytes(word), 9);
    }
}
