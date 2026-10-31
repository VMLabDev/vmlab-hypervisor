use crate::{Error, Result};

const PAGE_SIZE: usize = 4096;
static NEXT_IDENTITY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) fn next_identity() -> u64 {
    let id = NEXT_IDENTITY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    assert_ne!(id, 0, "memory identity exhausted");
    id
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeStamp {
    pub identity: u64,
    pub mapping: u64,
    pub first_page: u64,
    pub last_page: u64,
    pub generation: u64,
}

/// Physical accesses, including page table accesses, arrive here from Sail.
/// Implementations may dispatch MMIO, share RAM across harts, and track writes.
pub trait Memory {
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()>;
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()>;
    fn generation(&self) -> u64;
    /// Some promises side-effect-free RAM fetch, a unique memory-instance ID,
    /// and invalidation when any covered page or its mapping changes.
    /// Page identifiers belong to the returned mapping. Requests covering the
    /// same pages may share a guard when all stamp fields are equal.
    /// Shared implementations must serialize this query with their writes.
    fn code_stamp(&self, _address: u64, _len: usize) -> Option<CodeStamp> {
        None
    }
    /// Inclusive numeric bounds of the RAM region containing this address.
    /// Specialization must guard the resulting range and retain memory effects.
    fn ram_bounds(&self, _address: u64) -> Option<(u64, u64)> {
        None
    }
    /// Read and return the write generation as one atomic host operation.
    fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> Result<u64>;
    /// Validate the generation and write as one atomic host operation.
    fn store_conditional(&mut self, address: u64, bytes: &[u8], generation: u64) -> Result<bool>;
    /// Serializes a generated read-modify-write sequence with ordinary accesses.
    fn transaction_lock(&self) -> Option<std::sync::Arc<parking_lot::ReentrantMutex<()>>>;
}

#[derive(Debug)]
pub struct FlatMemory {
    pub base: u64,
    data: Vec<u8>,
    generation: u64,
    identity: u64,
    page_generations: Vec<u64>,
}

impl FlatMemory {
    pub fn new(base: u64, data: Vec<u8>) -> Self {
        let page_generations = vec![0; data.len().div_ceil(PAGE_SIZE)];
        Self {
            base,
            data,
            generation: 0,
            identity: next_identity(),
            page_generations,
        }
    }
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }
    fn range(&self, address: u64, len: usize) -> Result<std::ops::Range<usize>> {
        let start = address
            .checked_sub(self.base)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(Error::Memory(address))?;
        let end = start
            .checked_add(len)
            .filter(|n| *n <= self.data.len())
            .ok_or(Error::Memory(address))?;
        Ok(start..end)
    }
}

impl Memory for FlatMemory {
    fn ram_bounds(&self, address: u64) -> Option<(u64, u64)> {
        let end = self
            .base
            .checked_add(u64::try_from(self.data.len().checked_sub(1)?).ok()?)?;
        (address >= self.base && address <= end).then_some((self.base, end))
    }
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()> {
        bytes.copy_from_slice(&self.data[self.range(address, bytes.len())?]);
        Ok(())
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        let range = self.range(address, bytes.len())?;
        self.data[range.clone()].copy_from_slice(bytes);
        self.generation = self.generation.wrapping_add(1);
        if !range.is_empty() {
            let first = range.start / PAGE_SIZE;
            let last = (range.end - 1) / PAGE_SIZE;
            self.page_generations[first..=last].fill(self.generation);
        }
        Ok(())
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn code_stamp(&self, address: u64, len: usize) -> Option<CodeStamp> {
        let range = self.range(address, len).ok()?;
        if range.is_empty() {
            return None;
        }
        let first = range.start / PAGE_SIZE;
        let last = (range.end - 1) / PAGE_SIZE;
        Some(CodeStamp {
            identity: self.identity,
            mapping: self.base,
            first_page: first as u64,
            last_page: last as u64,
            generation: *self.page_generations[first..=last].iter().max()?,
        })
    }
    fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> Result<u64> {
        self.read(address, bytes)?;
        Ok(self.generation)
    }
    fn store_conditional(&mut self, address: u64, bytes: &[u8], generation: u64) -> Result<bool> {
        if self.generation != generation {
            return Ok(false);
        }
        self.write(address, bytes)?;
        Ok(true)
    }
    fn transaction_lock(&self) -> Option<std::sync::Arc<parking_lot::ReentrantMutex<()>>> {
        None
    }
}

/// Sequentially consistent shared RAM. Each host access is serialized, including
/// reservation validation and SC writes. Per-hart reservations remain in the CPU.
#[derive(Clone, Debug)]
pub struct SharedMemory {
    data: std::sync::Arc<std::sync::Mutex<FlatMemory>>,
    transaction: std::sync::Arc<parking_lot::ReentrantMutex<()>>,
}

impl SharedMemory {
    pub fn new(memory: FlatMemory) -> Self {
        Self {
            data: std::sync::Arc::new(std::sync::Mutex::new(memory)),
            transaction: std::sync::Arc::new(parking_lot::ReentrantMutex::new(())),
        }
    }
    fn with<T>(&self, f: impl FnOnce(&mut FlatMemory) -> T) -> T {
        let _transaction = self.transaction.lock();
        let mut memory = self
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut memory)
    }
}

impl Memory for SharedMemory {
    fn ram_bounds(&self, address: u64) -> Option<(u64, u64)> {
        self.with(|memory| memory.ram_bounds(address))
    }
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()> {
        self.with(|memory| memory.read(address, bytes))
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        self.with(|memory| memory.write(address, bytes))
    }
    fn generation(&self) -> u64 {
        self.with(|memory| memory.generation())
    }
    fn code_stamp(&self, address: u64, len: usize) -> Option<CodeStamp> {
        self.with(|memory| memory.code_stamp(address, len))
    }
    fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> Result<u64> {
        self.with(|memory| memory.load_reserved(address, bytes))
    }
    fn store_conditional(&mut self, address: u64, bytes: &[u8], generation: u64) -> Result<bool> {
        self.with(|memory| memory.store_conditional(address, bytes, generation))
    }
    fn transaction_lock(&self) -> Option<std::sync::Arc<parking_lot::ReentrantMutex<()>>> {
        Some(self.transaction.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_bounds_cover_the_region_without_artificial_chunk_boundaries() {
        let memory = FlatMemory::new(0x8000_0000, vec![0; 0x20000]);
        for address in [0x8000_0000, 0x8000_ffff, 0x8001_0000, 0x8001_ffff] {
            assert_eq!(memory.ram_bounds(address), Some((0x8000_0000, 0x8001_ffff)));
        }
        assert_eq!(memory.ram_bounds(0x7fff_ffff), None);
        assert_eq!(memory.ram_bounds(0x8002_0000), None);
        assert_eq!(FlatMemory::new(0, vec![]).ram_bounds(0), None);
        assert_eq!(
            FlatMemory::new(u64::MAX, vec![0]).ram_bounds(u64::MAX),
            Some((u64::MAX, u64::MAX))
        );
    }

    #[test]
    fn data_writes_preserve_code_but_cross_page_code_writes_invalidate() {
        let mut memory = FlatMemory::new(0x8000, vec![0; PAGE_SIZE * 3]);
        let first = memory.code_stamp(0x8000, 4).unwrap();
        let crossing = memory.code_stamp(0x8ffe, 4).unwrap();
        memory.write(0xa000, &[7]).unwrap();
        assert_eq!(memory.code_stamp(0x8000, 4), Some(first));
        assert_eq!(memory.code_stamp(0x8ffe, 4), Some(crossing));
        memory.write(0x9000, &[9]).unwrap();
        assert_eq!(memory.code_stamp(0x8000, 4), Some(first));
        assert_ne!(memory.code_stamp(0x8ffe, 4), Some(crossing));
        memory.write(0x8fff, &[1, 2]).unwrap();
        assert_ne!(memory.code_stamp(0x8000, 4), Some(first));
    }

    #[test]
    fn replacement_and_remapping_do_not_reuse_a_code_stamp() {
        let mut memory = FlatMemory::new(0x8000, vec![0; PAGE_SIZE * 2]);
        let old = memory.code_stamp(0x8004, 4).unwrap();
        let replacement = FlatMemory::new(0x8000, vec![0; PAGE_SIZE * 2]);
        assert_ne!(replacement.code_stamp(0x8004, 4), Some(old));
        memory.base = 0x7000;
        assert_ne!(memory.code_stamp(0x8004, 4), Some(old));
        assert!(memory.code_stamp(0x6000, 4).is_none());
    }
    #[test]
    fn bounded_access_and_code_generations() {
        let mut m = FlatMemory::new(0x8000, vec![0; 8]);
        assert!(m.write(0x7fff, &[1]).is_err());
        assert!(m.write(u64::MAX, &[1, 2]).is_err());
        assert!(m.write(0x8007, &[1, 2]).is_err());
        m.write(0x8000, &[3, 4]).unwrap();
        let mut bytes = [0; 2];
        m.read(0x8000, &mut bytes).unwrap();
        assert_eq!(bytes, [3, 4]);
        assert_eq!(m.generation(), 1);
    }

    #[test]
    fn other_hart_invalidates_reservation_and_sc_is_atomic() {
        let mut hart0 = SharedMemory::new(FlatMemory::new(0, vec![0; 8]));
        let mut hart1 = hart0.clone();
        let mut bytes = [0; 8];
        let generation = hart0.load_reserved(0, &mut bytes).unwrap();
        std::thread::spawn(move || hart1.write(0, &7u64.to_le_bytes()).unwrap())
            .join()
            .unwrap();
        assert!(
            !hart0
                .store_conditional(0, &9u64.to_le_bytes(), generation)
                .unwrap()
        );
        let generation = hart0.load_reserved(0, &mut bytes).unwrap();
        assert_eq!(u64::from_le_bytes(bytes), 7);
        assert!(
            hart0
                .store_conditional(0, &11u64.to_le_bytes(), generation)
                .unwrap()
        );
    }
}
