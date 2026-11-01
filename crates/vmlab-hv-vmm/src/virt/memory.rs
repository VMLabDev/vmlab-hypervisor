use parking_lot::{Mutex, ReentrantMutex};
use std::sync::Arc;
use vm_memory::{Bytes, GuestAddress};
use vmlab_hv_devices::GuestMemory;
use vmlab_hv_tcg::{CodeStamp, Error, Memory, Result};

use super::RAM_BASE;
use crate::{bus::MmioBus, pci::PciRoot};

const MAX_CODE_WINDOWS: usize = 262144;
const WINDOW_MAPPING: u64 = 1 << 63;

#[derive(Clone)]
struct ReadObservation {
    address: u64,
    len: usize,
    generation: u64,
    bytes: [u8; 16],
}

struct CodeWindow {
    bytes: [u8; 16],
    generation: u64,
    page_generation: u64,
}

#[derive(Default)]
struct CodeWindows {
    entries: std::collections::BTreeMap<(u64, usize), CodeWindow>,
    versions: std::collections::BTreeMap<(u64, usize, [u8; 16]), u64>,
    clock: u64,
}

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
    last_read: Option<ReadObservation>,
    code_windows: Arc<Mutex<CodeWindows>>,
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
            last_read: None,
            code_windows: Arc::default(),
        }
    }

    pub(super) fn map_flash(&mut self, base: u64, flash: Arc<Mutex<super::flash::Flash>>) {
        self.flash.push((base, flash));
    }

    pub(super) fn invalidate_code(&self) {
        let _guard = self.transaction.lock();
        self.cache_safe
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let mut windows = self.code_windows.lock();
        windows.entries.clear();
        windows.versions.clear();
    }

    fn is_ram(&self, address: u64, len: usize) -> bool {
        address >= RAM_BASE
            && address
                .checked_add(len as u64)
                .is_some_and(|end| end <= RAM_BASE + self.size)
    }

    fn access_generation(&self, address: u64, len: usize) -> Option<u64> {
        if len == 0 {
            return None;
        }
        if self.is_ram(address, len) {
            let first = ((address - RAM_BASE) / 4096) as usize;
            let last = ((address - RAM_BASE + len as u64 - 1) / 4096) as usize;
            return self.pages[first..=last]
                .iter()
                .map(|page| page.load(std::sync::atomic::Ordering::SeqCst))
                .max();
        }
        self.flash.iter().find_map(|(base, flash)| {
            (address >= *base
                && address
                    .checked_add(len as u64)
                    .is_some_and(|end| end <= base + (32 << 20)))
            .then(|| flash.lock().code_generation())
            .flatten()
        })
    }
}

impl PhysicalMemory {
    /// Validate precisely the fetched bytes. Firmware often keeps writable
    /// data beside code; changing that data must not invalidate the code.
    fn ram_code_generation(&self, address: u64, len: usize) -> Option<u64> {
        if len > 16 {
            return None;
        }
        let page_generation = self.access_generation(address, len)?;
        let mut windows = self.code_windows.lock();
        let key = (address, len);
        let mut actual = [0; 16];
        if self.cache_safe.load(std::sync::atomic::Ordering::SeqCst)
            && let Some(window) = windows.entries.get(&key)
            && window.page_generation == page_generation
        {
            actual = window.bytes;
        } else {
            self.ram
                .read_slice(&mut actual[..len], GuestAddress(address))
                .ok()?;
        }
        if let Some(read) = &self.last_read
            && (read.address, read.len) == (address, len)
            && actual[..len] != read.bytes[..len]
        {
            return None;
        }
        if let Some(window) = windows.entries.get_mut(&key)
            && window.bytes == actual
        {
            window.page_generation = page_generation;
            return Some(window.generation);
        }
        if windows.entries.len() == MAX_CODE_WINDOWS && !windows.entries.contains_key(&key) {
            windows.entries.pop_first();
        }
        // Reusing a previous byte image may reuse its translations. Keep exact
        // bytes rather than a hash so identity cannot alias different code.
        let version_key = (address, len, actual);
        let generation = if let Some(generation) = windows.versions.get(&version_key) {
            *generation
        } else {
            if windows.versions.len() == MAX_CODE_WINDOWS {
                windows.versions.pop_first();
            }
            windows.clock = windows
                .clock
                .checked_add(1)
                .expect("code window epoch exhausted");
            let generation = windows.clock;
            windows.versions.insert(version_key, generation);
            generation
        };
        windows.entries.insert(
            key,
            CodeWindow {
                bytes: actual,
                generation,
                page_generation,
            },
        );
        Some(generation)
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
        self.last_read = self
            .access_generation(address, bytes.len())
            .filter(|_| bytes.len() <= 16)
            .map(|generation| {
                let mut observed = [0; 16];
                observed[..bytes.len()].copy_from_slice(bytes);
                ReadObservation {
                    address,
                    len: bytes.len(),
                    generation,
                    bytes: observed,
                }
            });
        Ok(())
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        let _guard = self.transaction.lock();
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .wrapping_add(1);
        // These platform registers cannot initiate DMA. Any other MMIO write
        // disables the page-epoch shortcut before firmware can start PCI or
        // fw_cfg DMA. Device writes bypass this CPU memory adapter.
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
            .or_else(|| {
                self.flash.iter().find_map(|(base, _)| {
                    (*base..base + (32 << 20))
                        .contains(&address)
                        .then_some((*base, base + (32 << 20) - 1))
                })
            })
    }
    fn code_stamp(&self, address: u64, len: usize) -> Option<CodeStamp> {
        let _guard = self.transaction.lock();
        if len == 0 {
            return None;
        }
        if self.is_ram(address, len) {
            return Some(CodeStamp {
                identity: self.identity,
                // Distinct windows must not coalesce into one page guard.
                mapping: address | WINDOW_MAPPING,
                first_page: 0,
                last_page: 0,
                generation: self.ram_code_generation(address, len)?,
            });
        }
        let generation = self.access_generation(address, len)?;
        // The recorder asks for a stamp after its fetch. Do not attach a newer
        // generation to bytes read before another hart's intervening store.
        if self.last_read.as_ref().is_some_and(|read| {
            (read.address, read.len) == (address, len) && read.generation != generation
        }) {
            return None;
        }
        for (base, _) in &self.flash {
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
                    generation,
                });
            }
        }
        None
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
        memory_with_pages(4)
    }

    fn memory_with_pages(pages: usize) -> PhysicalMemory {
        let size = pages * 4096;
        let ram = GuestMemory::from_ranges(&[(GuestAddress(RAM_BASE), size)]).unwrap();
        PhysicalMemory::new(
            ram,
            size as u64,
            MmioBus::new(),
            Arc::new(Mutex::new(PciRoot::q35(0x4_0000_0000))),
        )
    }

    #[test]
    fn advertised_empty_virtio_mmio_transports_are_readable_after_cpu_restore() {
        use vmlab_hv_devices::riscv::virtio_mmio::EmptyVirtioMmio;
        use vmlab_hv_tcg::Riscv64;
        let mut memory = memory();
        for (i, word) in [0x0005e503u32, 0x0085e603, 0x00c5e683]
            .into_iter()
            .enumerate()
        {
            memory
                .write(RAM_BASE + i as u64 * 4, &word.to_le_bytes())
                .unwrap();
        }
        for slot in 1..=8 {
            memory
                .bus
                .lock()
                .register(
                    0x10000000 + slot * 0x1000,
                    0x1000,
                    Arc::new(Mutex::new(EmptyVirtioMmio)),
                )
                .unwrap();
        }
        let mut cpu = Riscv64::new(memory).unwrap();
        super::super::cpu::configure_memory(&mut cpu, 4 * 4096).unwrap();
        cpu.set_pc(RAM_BASE);
        let state = cpu.save_state();
        for slot in 1..=8 {
            cpu.restore_state(&state).unwrap();
            cpu.set_gpr(11, 0x10000000 + slot * 0x1000).unwrap();
            cpu.run(1).unwrap();
            assert_eq!(cpu.read_csr(0x342).unwrap(), 0, "slot {slot}");
            cpu.run(2).unwrap();
            assert_eq!(cpu.gpr(10).unwrap(), 0x74726976);
            assert_eq!(cpu.gpr(12).unwrap(), 0);
            assert_eq!(cpu.gpr(13).unwrap(), 0x554d4551);
        }
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
        assert_eq!(
            memory.ram_bounds(0x20000000),
            Some((0x20000000, 0x21ffffff))
        );
        let state = flash.lock().save_state().unwrap();
        memory.write(0x20000000, &[0x98]).unwrap();
        assert!(memory.code_stamp(0x20000000, 4).is_none());
        assert_eq!(
            memory.ram_bounds(0x20000000),
            Some((0x20000000, 0x21ffffff))
        );
        flash.lock().write(0, &[0xff]);
        assert_ne!(memory.code_stamp(0x20000000, 4), Some(initial));
        let array = memory.code_stamp(0x20000000, 4).unwrap();
        flash.lock().restore_state(&state).unwrap();
        assert_ne!(memory.code_stamp(0x20000000, 4), Some(array));
        memory.write(0x30008004, &7u16.to_le_bytes()).unwrap();
        assert!(memory.code_stamp(RAM_BASE, 4).is_some());
        assert!(memory.code_stamp(0x20000000, 4).is_some());
        assert!(memory.code_stamp(0x21ffffff, 2).is_none());
    }

    #[test]
    fn restoring_ram_discards_old_window_epochs() {
        let memory = memory();
        let before = memory.code_stamp(RAM_BASE, 4).unwrap();
        memory
            .ram
            .write_slice(&[1], GuestAddress(RAM_BASE))
            .unwrap();
        memory.invalidate_code();
        let restored = memory.code_stamp(RAM_BASE, 4).unwrap();
        assert_ne!(restored, before);
        assert_eq!(memory.code_stamp(RAM_BASE, 4), Some(restored));
        memory
            .ram
            .write_slice(&[2], GuestAddress(RAM_BASE))
            .unwrap();
        assert_ne!(memory.code_stamp(RAM_BASE, 4), Some(restored));
    }

    #[test]
    fn a_store_between_fetch_and_stamp_cannot_validate_the_old_instruction() {
        let mut memory = memory();
        let mut peer = memory.clone();
        memory
            .write(RAM_BASE, &0x00108093u32.to_le_bytes())
            .unwrap();
        let mut instruction = [0; 4];
        memory.read(RAM_BASE, &mut instruction).unwrap();
        peer.write(RAM_BASE, &0x00208093u32.to_le_bytes()).unwrap();
        assert!(memory.code_stamp(RAM_BASE, 4).is_none());
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
    fn possible_dma_switches_to_exact_instruction_validation() {
        let mut memory = memory();
        let other = memory.clone();
        memory.write(0x2000000, &1u32.to_le_bytes()).unwrap();
        memory.write(0x10000000, &[65]).unwrap();
        assert!(other.code_stamp(RAM_BASE, 4).is_some());
        memory.write(0x30008004, &7u16.to_le_bytes()).unwrap();
        let enabled = other.code_stamp(RAM_BASE, 4).unwrap();
        memory
            .ram
            .write_slice(&[2], GuestAddress(RAM_BASE))
            .unwrap();
        let changed = other.code_stamp(RAM_BASE, 4).unwrap();
        assert_ne!(enabled, changed);
        assert_eq!(other.code_stamp(RAM_BASE, 4), Some(changed));
        memory.write(RAM_BASE, &[1]).unwrap();
        assert_ne!(other.code_stamp(RAM_BASE, 4), Some(changed));
    }

    #[test]
    fn nearby_data_stores_do_not_retrain_a_warm_instruction_loop() {
        nearby_data_stores(false);
        nearby_data_stores(true);
    }

    fn nearby_data_stores(dma: bool) {
        let mut memory = memory();
        for (index, word) in [0x00000117u32, 0x00108093, 0x08113023, 0xff9ff06f]
            .into_iter()
            .enumerate()
        {
            memory
                .write(RAM_BASE + index as u64 * 4, &word.to_le_bytes())
                .unwrap();
        }
        if dma {
            memory.write(0x30008004, &[7]).unwrap();
        }
        let mut cpu = vmlab_hv_tcg::Riscv64::new(memory).unwrap();
        cpu.specialize = true;
        cpu.interpreter.operation_limit = u64::MAX;
        cpu.set_pc(RAM_BASE);
        cpu.run(1).unwrap();
        cpu.run(192).unwrap();
        let trained = cpu.specialization_stats.recordings;
        cpu.run(192).unwrap();
        assert_eq!(cpu.gpr(1).unwrap(), 128);
        let retrained = cpu.specialization_stats.recordings - trained;
        eprintln!("unchanged warm loop: {retrained} retrainings over 64 iterations");
        assert_eq!(retrained, 0, "unchanged instructions were retrained");
    }

    #[test]
    fn overlapping_instruction_windows_have_independent_guards() {
        let mut memory = memory();
        memory.write(RAM_BASE, &[1, 2, 3, 4]).unwrap();
        let page = memory.code_stamp(RAM_BASE, 4).unwrap();
        memory.write(0x30008004, &[7]).unwrap();
        let prefix = memory.code_stamp(RAM_BASE, 2).unwrap();
        let whole = memory.code_stamp(RAM_BASE, 4).unwrap();
        let suffix = memory.code_stamp(RAM_BASE + 2, 2).unwrap();
        assert_eq!(page, whole);
        assert_ne!(prefix.generation, whole.generation);
        assert_ne!(prefix.mapping, suffix.mapping);
        memory
            .ram
            .write_slice(&[5, 6], GuestAddress(RAM_BASE + 2))
            .unwrap();
        assert_eq!(memory.code_stamp(RAM_BASE, 2), Some(prefix));
        assert_ne!(memory.code_stamp(RAM_BASE, 4), Some(whole));
        assert_ne!(memory.code_stamp(RAM_BASE + 2, 2), Some(suffix));
    }

    #[test]
    fn dma_to_data_near_code_does_not_invalidate_unchanged_instructions() {
        let mut memory = memory();
        memory
            .write(RAM_BASE, &0x00108093u32.to_le_bytes())
            .unwrap();
        memory.write(0x30008004, &[7]).unwrap();
        let instruction = memory.code_stamp(RAM_BASE, 4).unwrap();
        memory
            .ram
            .write_slice(&[1], GuestAddress(RAM_BASE + 128))
            .unwrap();
        assert_eq!(memory.code_stamp(RAM_BASE, 4), Some(instruction));
    }

    #[test]
    fn dma_between_fetch_and_stamp_does_not_fold_stale_bytes() {
        let mut memory = memory();
        memory.write(0x30008004, &[7]).unwrap();
        memory.write(RAM_BASE + 4094, &[1, 2, 3, 4]).unwrap();
        let mut instruction = [0; 4];
        memory.read(RAM_BASE + 4094, &mut instruction).unwrap();
        memory
            .ram
            .write_slice(&[5, 6], GuestAddress(RAM_BASE + 4096))
            .unwrap();
        assert!(memory.code_stamp(RAM_BASE + 4094, 4).is_none());
        memory.read(RAM_BASE + 4094, &mut instruction).unwrap();
        assert_eq!(instruction, [1, 2, 5, 6]);
        let stamp = memory.code_stamp(RAM_BASE + 4094, 4).unwrap();
        assert_eq!(memory.code_stamp(RAM_BASE + 4094, 4), Some(stamp));
    }

    #[test]
    fn dma_code_tracking_is_bounded_and_eviction_changes_the_stamp() {
        let mut memory = memory_with_pages((MAX_CODE_WINDOWS * 2).div_ceil(4096) + 1);
        memory.write(0x30008004, &[7]).unwrap();
        let first = memory.code_stamp(RAM_BASE, 2).unwrap();
        for window in 1..=MAX_CODE_WINDOWS {
            memory.code_stamp(RAM_BASE + window as u64 * 2, 2).unwrap();
        }
        assert_eq!(memory.code_windows.lock().entries.len(), MAX_CODE_WINDOWS);
        assert_ne!(memory.code_stamp(RAM_BASE, 2), Some(first));
        assert_eq!(memory.code_windows.lock().entries.len(), MAX_CODE_WINDOWS);
        assert_eq!(memory.code_windows.lock().versions.len(), MAX_CODE_WINDOWS);
    }

    #[test]
    fn recurring_code_image_history_has_a_global_bound() {
        let mut memory = memory();
        memory.write(RAM_BASE, &0u32.to_le_bytes()).unwrap();
        let first = memory.code_stamp(RAM_BASE, 4).unwrap();
        for version in 1..=MAX_CODE_WINDOWS as u32 {
            memory.write(RAM_BASE, &version.to_le_bytes()).unwrap();
            memory.code_stamp(RAM_BASE, 4).unwrap();
        }
        assert_eq!(memory.code_windows.lock().entries.len(), 1);
        assert_eq!(memory.code_windows.lock().versions.len(), MAX_CODE_WINDOWS);
        memory.write(RAM_BASE, &0u32.to_le_bytes()).unwrap();
        assert_ne!(memory.code_stamp(RAM_BASE, 4), Some(first));
        assert_eq!(memory.code_windows.lock().versions.len(), MAX_CODE_WINDOWS);
    }

    #[test]
    fn execution_observes_cpu_and_dma_replacement_of_cached_instructions() {
        instruction_replacement(false);
        instruction_replacement(true);
    }

    fn instruction_replacement(dma: bool) {
        let mut memory = memory();
        memory
            .write(RAM_BASE, &0x00108093u32.to_le_bytes())
            .unwrap();
        memory
            .write(RAM_BASE + 4, &0xffdff06fu32.to_le_bytes())
            .unwrap();
        if dma {
            memory.write(0x30008004, &[7]).unwrap();
        }
        let mut peer = memory.clone();
        let mut cpu = vmlab_hv_tcg::Riscv64::new(memory).unwrap();
        cpu.specialize = true;
        cpu.set_pc(RAM_BASE);
        cpu.run(64).unwrap();
        assert_eq!(cpu.gpr(1).unwrap(), 32);
        assert!(cpu.specialization_stats.hits > 0);
        if dma {
            peer.ram
                .write_slice(&0x00208093u32.to_le_bytes(), GuestAddress(RAM_BASE))
                .unwrap();
        } else {
            peer.write(RAM_BASE, &0x00208093u32.to_le_bytes()).unwrap();
        }
        cpu.run(64).unwrap();
        assert_eq!(cpu.gpr(1).unwrap(), 96);
    }

    #[test]
    fn alternating_instruction_images_reuse_warmed_translations() {
        for dma in [false, true] {
            let make = || {
                let mut memory = memory();
                memory
                    .write(RAM_BASE, &0x00108093u32.to_le_bytes())
                    .unwrap();
                memory
                    .write(RAM_BASE + 4, &0xffdff06fu32.to_le_bytes())
                    .unwrap();
                if dma {
                    memory.write(0x30008004, &[7]).unwrap();
                }
                let mut cpu = vmlab_hv_tcg::Riscv64::new(memory).unwrap();
                cpu.use_external_clock().unwrap();
                cpu.interpreter.operation_limit = u64::MAX;
                cpu.set_pc(RAM_BASE);
                cpu
            };
            let replace = |cpu: &mut vmlab_hv_tcg::Riscv64<PhysicalMemory>, turn: u32| {
                let word = (0x00108093 + (turn % 2) * 0x100000).to_le_bytes();
                if dma {
                    cpu.interpreter
                        .memory
                        .ram
                        .write_slice(&word, GuestAddress(RAM_BASE))
                        .unwrap();
                } else {
                    cpu.interpreter.memory.write(RAM_BASE, &word).unwrap();
                }
            };
            let mut specialized = make();
            specialized.specialize = true;
            for turn in 0..8 {
                replace(&mut specialized, turn);
                specialized.run(192).unwrap();
            }
            let mut reference = make();
            for (name, value) in specialized.interpreter.globals.iter() {
                reference
                    .interpreter
                    .globals
                    .insert(name.clone(), value.clone());
            }
            reference.steps = specialized.steps;
            let operations = specialized.interpreter.operations;
            let recordings = specialized.specialization_stats.recordings;
            for turn in 0..8 {
                replace(&mut reference, turn);
                replace(&mut specialized, turn);
                reference.run(192).unwrap();
                specialized.run(192).unwrap();
                assert_eq!(
                    *specialized.interpreter.globals, *reference.interpreter.globals,
                    "dma={dma} turn={turn}"
                );
            }
            assert_eq!(
                specialized.interpreter.operations, operations,
                "dma={dma}: recurring code image returned to Sail"
            );
            assert_eq!(specialized.specialization_stats.recordings, recordings);
        }
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
