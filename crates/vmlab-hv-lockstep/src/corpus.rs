//! ELF64 little-endian RISC-V executable loader and tohost test driver.
use crate::{CpuError, Memory, RunError, State, StepCpu, run_until};

pub struct TestImage {
    pub memory: Memory,
    pub entry: u64,
    pub tohost: u64,
}

struct Elf<'a>(&'a [u8]);
impl<'a> Elf<'a> {
    fn bytes(&self, offset: u64, len: u64) -> Result<&'a [u8], CpuError> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| CpuError("ELF range overflow".into()))?;
        let range = usize::try_from(offset)
            .ok()
            .zip(usize::try_from(end).ok())
            .and_then(|(a, b)| self.0.get(a..b));
        range.ok_or_else(|| CpuError("truncated ELF".into()))
    }
    fn u16(&self, at: u64) -> Result<u16, CpuError> {
        Ok(u16::from_le_bytes(
            self.bytes(at, 2)?.try_into().expect("length checked"),
        ))
    }
    fn u32(&self, at: u64) -> Result<u32, CpuError> {
        Ok(u32::from_le_bytes(
            self.bytes(at, 4)?.try_into().expect("length checked"),
        ))
    }
    fn u64(&self, at: u64) -> Result<u64, CpuError> {
        Ok(u64::from_le_bytes(
            self.bytes(at, 8)?.try_into().expect("length checked"),
        ))
    }
    fn table(
        &self,
        base: u64,
        stride: u64,
        count: u64,
        minimum: u64,
    ) -> Result<Vec<u64>, CpuError> {
        if stride < minimum {
            return Err(CpuError("invalid ELF table stride".into()));
        }
        self.bytes(
            base,
            stride
                .checked_mul(count)
                .ok_or_else(|| CpuError("ELF table overflow".into()))?,
        )?;
        Ok((0..count).map(|i| base + stride * i).collect())
    }
}

impl TestImage {
    /// Load PT_LOAD segments, zero BSS, and find `tohost` in SHT_SYMTAB.
    /// The bounded RAM allocation rejects hostile sparse ELF files.
    pub fn elf(bytes: &[u8], base: u64, size: usize) -> Result<Self, CpuError> {
        if size > 256 * 1024 * 1024 {
            return Err(CpuError("corpus RAM exceeds 256 MiB".into()));
        }
        let elf = Elf(bytes);
        if elf.bytes(0, 7)? != b"\x7fELF\x02\x01\x01"
            || elf.u16(16)? != 2
            || elf.u16(18)? != 243
            || elf.u32(20)? != 1
        {
            return Err(CpuError(
                "expected little-endian RV64 executable ELF".into(),
            ));
        }
        if elf.u16(52)? != 64 {
            return Err(CpuError("invalid ELF header size".into()));
        }
        let entry = elf.u64(24)?;
        let mut memory = Memory::new(base, vec![0; size])?;
        let segments = elf.table(elf.u64(32)?, elf.u16(54)? as u64, elf.u16(56)? as u64, 56)?;
        let mut executable_entry = false;
        let mut loaded = Vec::new();
        for at in segments {
            if elf.u32(at)? != 1 {
                continue;
            }
            let offset = elf.u64(at + 8)?;
            let virtual_address = elf.u64(at + 16)?;
            let address = elf.u64(at + 24)?;
            let filesz = elf.u64(at + 32)?;
            let memsz = elf.u64(at + 40)?;
            if filesz > memsz || virtual_address != address {
                return Err(CpuError("unsupported ELF segment mapping".into()));
            }
            let len = usize::try_from(memsz).map_err(|e| CpuError(e.to_string()))?;
            memory.read(address, len)?;
            let end = address
                .checked_add(memsz)
                .ok_or_else(|| CpuError("ELF segment overflow".into()))?;
            if loaded.iter().any(|&(a, b)| address < b && a < end) {
                return Err(CpuError("overlapping ELF segments".into()));
            }
            loaded.push((address, end));
            if address <= entry && entry < end && elf.u32(at + 4)? & 1 != 0 {
                executable_entry = true;
            }
            memory.write(address, elf.bytes(offset, filesz)?)?;
        }
        if !executable_entry {
            return Err(CpuError("ELF entry is not executable".into()));
        }
        let sections = elf.table(elf.u64(40)?, elf.u16(58)? as u64, elf.u16(60)? as u64, 64)?;
        let mut tohost = None;
        for &at in &sections {
            if elf.u32(at + 4)? != 2 {
                continue;
            }
            let link = elf.u32(at + 40)? as usize;
            let strings = *sections
                .get(link)
                .ok_or_else(|| CpuError("bad ELF string table link".into()))?;
            if elf.u32(strings + 4)? != 3 {
                return Err(CpuError("ELF symbols require a string table".into()));
            }
            let strings = elf.bytes(elf.u64(strings + 24)?, elf.u64(strings + 32)?)?;
            let stride = elf.u64(at + 56)?;
            let size = elf.u64(at + 32)?;
            if stride < 24 || size % stride != 0 {
                return Err(CpuError("bad ELF symbol table".into()));
            }
            for symbol in elf.table(elf.u64(at + 24)?, stride, size / stride, 24)? {
                let name = strings
                    .get(elf.u32(symbol)? as usize..)
                    .ok_or_else(|| CpuError("bad ELF symbol name".into()))?;
                let end = name
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or_else(|| CpuError("unterminated ELF symbol".into()))?;
                if &name[..end] == b"tohost" && elf.u16(symbol + 6)? != 0 {
                    tohost = Some(elf.u64(symbol + 8)?);
                }
            }
        }
        let tohost = tohost.ok_or_else(|| {
            CpuError(
                "ELF has no defined tohost symbol; use a physical-mode test environment".into(),
            )
        })?;
        memory.read(tohost, 8)?;
        memory.take_writes();
        Ok(Self {
            memory,
            entry,
            tohost,
        })
    }

    pub fn run(
        &self,
        left: &mut impl StepCpu,
        right: &mut impl StepCpu,
        initial: &State,
        limit: u64,
    ) -> Result<u64, RunError> {
        let mut state = initial.clone();
        state.pc = self.entry;
        let outcome = run_until(left, right, &state, &self.memory, limit, |memory| {
            let value = u64::from_le_bytes(
                memory
                    .read(self.tohost, 8)?
                    .try_into()
                    .expect("length checked"),
            );
            if value == 0 {
                return Ok(false);
            }
            if value != 1 {
                return Err(CpuError(format!(
                    "corpus test failed: tohost={value:#x}, test={}",
                    value >> 1
                )));
            }
            Ok(true)
        })?;
        if !outcome.halted {
            return Err(RunError::Cpu {
                side: "corpus",
                step: limit,
                source: CpuError("instruction limit reached before tohost pass".into()),
            });
        }
        Ok(outcome.steps)
    }
}
