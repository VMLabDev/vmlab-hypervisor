//! A strict single-hart replay of captured RAM and array-mode firmware.

use super::Result;
use crate::{
    bus::MmioBus,
    pci::PciRoot,
    virt::{RAM_BASE, flash::Flash, memory::PhysicalMemory},
};
use parking_lot::{Mutex, ReentrantMutex};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::File,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use vm_memory::{Bytes, GuestAddress};
use vmlab_hv_devices::{GuestMemory, Snapshot};
use vmlab_hv_tcg::{CodeStamp, CpuState, Error, Memory, Riscv64};

#[derive(Deserialize)]
struct Fixture {
    sha256: String,
    cpu: PathBuf,
    ram: Ram,
    roms: Vec<Rom>,
}

#[derive(Deserialize)]
struct Ram {
    base: u64,
    length: u64,
    path: PathBuf,
    runs: Vec<(u64, u64)>,
}
#[derive(Deserialize)]
struct Rom {
    base: u64,
    path: PathBuf,
    state: PathBuf,
}

#[derive(Clone)]
struct ReplayMemory {
    physical: PhysicalMemory,
    dirty: Arc<Mutex<BTreeSet<u64>>>,
}

impl ReplayMemory {
    fn ram(&self, address: u64, len: usize) -> bool {
        address >= RAM_BASE
            && address
                .checked_add(len as u64)
                .is_some_and(|end| end <= RAM_BASE + self.physical.size)
    }
    fn readable(&self, address: u64, len: usize) -> bool {
        self.ram(address, len)
            || (address >= 0x20000000
                && address
                    .checked_add(len as u64)
                    .is_some_and(|end| end <= 0x24000000))
    }
    fn track(&self, address: u64, len: usize) {
        if len != 0 {
            self.dirty
                .lock()
                .extend((address - RAM_BASE) / 4096..=(address - RAM_BASE + len as u64 - 1) / 4096);
        }
    }
}

impl Memory for ReplayMemory {
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> vmlab_hv_tcg::Result<()> {
        if !self.readable(address, bytes.len()) {
            return Err(Error::Memory(address));
        }
        self.physical.read(address, bytes)
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> vmlab_hv_tcg::Result<()> {
        if !self.ram(address, bytes.len()) {
            return Err(Error::Memory(address));
        }
        self.track(address, bytes.len());
        self.physical.write(address, bytes)
    }
    fn generation(&self) -> u64 {
        self.physical.generation()
    }
    fn code_stamp(&self, address: u64, len: usize) -> Option<CodeStamp> {
        self.physical.code_stamp(address, len)
    }
    fn code_epoch(&self) -> Option<(u64, u64)> {
        self.physical.code_epoch()
    }
    fn code_epoch_for(&self, ranges: &[(u64, usize)]) -> Option<(u64, u64)> {
        self.physical.code_epoch_for(ranges)
    }
    fn ram_bounds(&self, address: u64) -> Option<(u64, u64)> {
        self.physical.ram_bounds(address)
    }
    fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> vmlab_hv_tcg::Result<u64> {
        if !self.ram(address, bytes.len()) {
            return Err(Error::Memory(address));
        }
        self.physical.load_reserved(address, bytes)
    }
    fn store_conditional(
        &mut self,
        address: u64,
        bytes: &[u8],
        generation: u64,
    ) -> vmlab_hv_tcg::Result<bool> {
        if !self.ram(address, bytes.len()) {
            return Err(Error::Memory(address));
        }
        self.track(address, bytes.len());
        self.physical.store_conditional(address, bytes, generation)
    }
    fn transaction_lock(&self) -> Option<Arc<ReentrantMutex<()>>> {
        self.physical.transaction_lock()
    }
}

fn thread_cpu_time() -> Result<f64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: the pointer names a writable timespec and the clock ID selects
    // the calling thread; no guest pointer crosses this host ABI.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(time.tv_sec as f64 + time.tv_nsec as f64 / 1e9)
}

fn digest_file(file: &File, length: u64, digest: &mut Sha256) -> Result<()> {
    let mut buffer = [0; 65536];
    let mut offset = 0;
    while offset < length {
        let count = buffer.len().min((length - offset) as usize);
        file.read_exact_at(&mut buffer[..count], offset)?;
        digest.update(&buffer[..count]);
        offset += count as u64;
    }
    Ok(())
}

fn outcome(cpu: &Riscv64<ReplayMemory>) -> Result<(String, String)> {
    let state = serde_json::to_value(cpu.save_state())?;
    if !state["reservation"].is_null() {
        return Err("replay boundary retains an LR/SC reservation".into());
    }
    let state_digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&state)?));
    let mut writes = Sha256::new();
    let mut bytes = [0; 4096];
    for page in cpu.interpreter.memory.dirty.lock().iter() {
        writes.update(page.to_le_bytes());
        cpu.interpreter
            .memory
            .physical
            .ram
            .read_slice(&mut bytes, GuestAddress(RAM_BASE + page * 4096))?;
        writes.update(bytes);
    }
    Ok((state_digest, format!("{:x}", writes.finalize())))
}

fn execute(cpu: &mut Riscv64<ReplayMemory>, steps: u64) -> Result<()> {
    let (completed, waiting) = cpu.run(steps)?;
    if completed != steps || waiting {
        return Err("replay interval reached WFI before its fixed endpoint".into());
    }
    Ok(())
}

/// Refuse uncaptured IO; compare every CPU global and dirtied RAM page against
/// source execution before timing the same interval with warmed translations.
pub fn measure_replay(directory: &Path, steps: u64, rounds: u64) -> Result<serde_json::Value> {
    if !(1..=1_000_000).contains(&steps) || !(1..=1024).contains(&rounds) {
        return Err("replay steps must be in 1..=1000000 and rounds in 1..=1024".into());
    }
    let init = Instant::now();
    let fixture: Fixture =
        serde_json::from_slice(&std::fs::read(directory.join("manifest.json"))?)?;
    if fixture.ram.base != RAM_BASE
        || fixture.ram.length == 0
        || fixture.ram.length > 14336 << 20
        || !fixture.ram.length.is_multiple_of(4096)
    {
        return Err("invalid replay RAM extent".into());
    }
    let raw_cpu = std::fs::read(&fixture.cpu)?;
    let state_metadata: serde_json::Value = serde_json::from_slice(&raw_cpu)?;
    if !state_metadata["reservation"].is_null() {
        return Err("initial replay boundary retains an LR/SC reservation".into());
    }
    let state: CpuState = serde_json::from_slice(&raw_cpu)?;
    let mut digest = Sha256::new();
    digest.update(&raw_cpu);
    let ram = GuestMemory::from_ranges(&[(GuestAddress(RAM_BASE), fixture.ram.length as usize)])?;
    let pci = Arc::new(Mutex::new(PciRoot::new(0x4_00000000)));
    let mut physical = PhysicalMemory::new(ram, fixture.ram.length, MmioBus::new(), pci);
    if fixture.roms.len() != 2
        || fixture.roms[0].base != 0x20000000
        || fixture.roms[1].base != 0x22000000
    {
        return Err("replay requires both captured 32 MiB pflash banks".into());
    }
    for rom in &fixture.roms {
        let raw = File::open(&rom.path)?;
        if raw.metadata()?.len() != 32 << 20 {
            return Err("invalid replay ROM extent".into());
        }
        digest_file(&raw, 32 << 20, &mut digest)?;
        let mut flash = Flash::open("replay", &rom.path, false)?;
        flash.restore_state(&std::fs::read(&rom.state)?)?;
        if flash.code_generation().is_none() {
            return Err("captured pflash is not in array mode".into());
        }
        let flash = Arc::new(Mutex::new(flash));
        physical
            .bus
            .lock()
            .register(rom.base, 32 << 20, flash.clone())?;
        physical.map_flash(rom.base, flash);
    }
    let initial_ram = File::open(&fixture.ram.path)?;
    if initial_ram.metadata()?.len() != fixture.ram.length {
        return Err("replay RAM file size mismatch".into());
    }
    let mut previous_end = 0;
    for (offset, length) in &fixture.ram.runs {
        if *length == 0
            || !offset.is_multiple_of(4096)
            || !length.is_multiple_of(4096)
            || *offset < previous_end
            || offset
                .checked_add(*length)
                .is_none_or(|end| end > fixture.ram.length)
        {
            return Err("invalid replay RAM data run".into());
        }
        digest.update(offset.to_le_bytes());
        digest.update(length.to_le_bytes());
        let mut position = *offset;
        let mut bytes = [0; 65536];
        while position < offset + length {
            let count = bytes.len().min((offset + length - position) as usize);
            initial_ram.read_exact_at(&mut bytes[..count], position)?;
            digest.update(&bytes[..count]);
            physical
                .ram
                .write_slice(&bytes[..count], GuestAddress(RAM_BASE + position))?;
            position += count as u64;
        }
        previous_end = offset + length;
    }
    if format!("{:x}", digest.finalize()) != fixture.sha256 {
        return Err("replay fixture digest mismatch".into());
    }
    let memory = ReplayMemory {
        physical,
        dirty: Arc::default(),
    };
    let mut cpu = Riscv64::new(memory)?;
    cpu.use_external_clock()?;
    cpu.restore_state(&state)?;
    cpu.specialize = true;
    cpu.interpreter.operation_limit = u64::MAX;
    let globals = (*cpu.interpreter.globals).clone();
    let initial_steps = cpu.steps;
    let initialization_seconds = init.elapsed().as_secs_f64();
    let reset = |cpu: &mut Riscv64<ReplayMemory>| -> Result<()> {
        let pages = std::mem::take(&mut *cpu.interpreter.memory.dirty.lock());
        let mut bytes = [0; 4096];
        for page in pages {
            initial_ram.read_exact_at(&mut bytes, page * 4096)?;
            cpu.interpreter
                .memory
                .physical
                .write(RAM_BASE + page * 4096, &bytes)?;
        }
        for (name, value) in &globals {
            cpu.interpreter.globals.insert(name.clone(), value.clone());
        }
        cpu.steps = initial_steps;
        Ok(())
    };
    let warm = Instant::now();
    execute(&mut cpu, steps)?;
    let expected = outcome(&cpu)?;
    reset(&mut cpu)?;
    cpu.specialize = false;
    execute(&mut cpu, steps)?;
    if outcome(&cpu)? != expected {
        return Err("captured replay differs from source CPU or RAM state".into());
    }
    reset(&mut cpu)?;
    cpu.specialize = true;
    execute(&mut cpu, steps)?;
    if outcome(&cpu)? != expected {
        return Err("captured replay warmup is nondeterministic".into());
    }
    reset(&mut cpu)?;
    let warmup_seconds = warm.elapsed().as_secs_f64();
    let native = cpu.specialization_stats.hits;
    let recordings = cpu.specialization_stats.recordings;
    let evictions = cpu.specialization_stats.evictions;
    let mut seconds = 0.0;
    let mut cpu_seconds = 0.0;
    let mut instructions = 0;
    let mut measured_operations = 0;
    for round in 0..rounds {
        if round != 0 {
            reset(&mut cpu)?;
        }
        let retired = cpu.read_csr(0xb02)?;
        let operations = cpu.interpreter.operations;
        let cpu_start = thread_cpu_time()?;
        let start = Instant::now();
        execute(&mut cpu, steps)?;
        seconds += start.elapsed().as_secs_f64();
        cpu_seconds += thread_cpu_time()? - cpu_start;
        measured_operations += cpu.interpreter.operations - operations;
        if outcome(&cpu)? != expected {
            return Err("timed replay differs from verified CPU or RAM state".into());
        }
        instructions += cpu.read_csr(0xb02)?.wrapping_sub(retired);
    }
    let (cache_units, cache_limit) = cpu.trace_cache_usage();
    Ok(serde_json::json!({
        "instructions": instructions, "steps": steps * rounds, "rounds": rounds, "seconds": seconds, "mips": instructions as f64 / seconds / 1e6,
        "native_instructions": cpu.specialization_stats.hits - native, "ir_operations": measured_operations,
        "cpu_seconds": cpu_seconds, "cpu_mips": instructions as f64 / cpu_seconds / 1e6,
        "recordings": cpu.specialization_stats.recordings - recordings, "cache_evictions": cpu.specialization_stats.evictions - evictions,
        "cache_units": cache_units, "cache_limit": cache_limit,
        "dirty_pages": cpu.interpreter.memory.dirty.lock().len(), "state_sha256": expected.0, "writes_sha256": expected.1,
        "fixture_sha256": fixture.sha256, "pc": cpu.pc()?,
        "initialization_seconds": initialization_seconds, "warmup_seconds": warmup_seconds,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_rejects_invalid_limits_before_opening_a_fixture() {
        for (steps, rounds) in [(0, 1), (1_000_001, 1), (1, 0), (1, 1025)] {
            assert!(
                measure_replay(Path::new("missing-replay-fixture"), steps, rounds)
                    .unwrap_err()
                    .to_string()
                    .contains("replay steps")
            );
        }
    }

    #[test]
    fn replay_refuses_uncaptured_io_and_rom_writes() {
        let ram = GuestMemory::from_ranges(&[(GuestAddress(RAM_BASE), 4096)]).unwrap();
        let physical = PhysicalMemory::new(
            ram,
            4096,
            MmioBus::new(),
            Arc::new(Mutex::new(PciRoot::new(0x4_00000000))),
        );
        let mut memory = ReplayMemory {
            physical,
            dirty: Arc::default(),
        };
        let mut bytes = [0; 4];
        assert!(memory.read(0x10000000, &mut bytes).is_err());
        assert!(memory.read(RAM_BASE + 4094, &mut bytes).is_err());
        assert!(memory.write(0x20000000, &[1]).is_err());
        assert!(memory.load_reserved(0x2000000, &mut bytes).is_err());
        assert!(memory.store_conditional(0x2000000, &[1], 0).is_err());
        memory.write(RAM_BASE, &[42]).unwrap();
        memory.read(RAM_BASE, &mut bytes).unwrap();
        assert_eq!(bytes, [42, 0, 0, 0]);
        assert_eq!(*memory.dirty.lock(), BTreeSet::from([0]));
    }
}
