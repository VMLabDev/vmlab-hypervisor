use crate::{Error, Result};

/// Physical accesses, including page table accesses, arrive here from Sail.
/// Implementations may dispatch MMIO, share RAM across harts, and track writes.
pub trait Memory {
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()>;
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()>;
    fn generation(&self) -> u64;
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
}

impl FlatMemory {
    pub fn new(base: u64, data: Vec<u8>) -> Self {
        Self {
            base,
            data,
            generation: 0,
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
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()> {
        bytes.copy_from_slice(&self.data[self.range(address, bytes.len())?]);
        Ok(())
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        let range = self.range(address, bytes.len())?;
        self.data[range].copy_from_slice(bytes);
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }
    fn generation(&self) -> u64 {
        self.generation
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
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()> {
        self.with(|memory| memory.read(address, bytes))
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        self.with(|memory| memory.write(address, bytes))
    }
    fn generation(&self) -> u64 {
        self.with(|memory| memory.generation())
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
