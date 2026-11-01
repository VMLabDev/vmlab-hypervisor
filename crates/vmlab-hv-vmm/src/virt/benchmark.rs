//! Bounded CPU workloads using virt's memory adapter and platform devices.

use std::{error::Error, sync::Arc, time::Instant};

use parking_lot::Mutex;
use vm_memory::GuestAddress;
use vmlab_hv_devices::{GuestMemory, riscv::Clint};
use vmlab_hv_tcg::{Memory, Privilege, Riscv64, Value};

use super::{RAM_BASE, cpu::configure_memory, memory::PhysicalMemory};
use crate::{bus::MmioBus, pci::PciRoot};

mod replay;
pub use replay::measure_replay;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const RAM_SIZE: u64 = 16 << 20;
const HANDLER: u64 = RAM_BASE + 0x100;
const CLINT: u64 = 0x2000000;

struct Workload {
    cpu: Riscv64<PhysicalMemory>,
    clint: Arc<Mutex<Clint>>,
    kind: String,
    events: u64,
    expected: u64,
}

impl Workload {
    fn new(kind: &str) -> Result<Self> {
        let ram = GuestMemory::from_ranges(&[(GuestAddress(RAM_BASE), RAM_SIZE as usize)])?;
        let clint = Arc::new(Mutex::new(Clint::new(1)));
        let mut bus = MmioBus::new();
        bus.register(CLINT, 0x10000, clint.clone())?;
        let pci = Arc::new(Mutex::new(PciRoot::new(0x4_0000_0000)));
        let memory = PhysicalMemory::new(ram, RAM_SIZE, bus, pci);
        let mut cpu = Riscv64::new(memory)?;
        configure_memory(&mut cpu, RAM_SIZE)?;
        cpu.use_external_clock()?;
        cpu.interpreter
            .globals
            .insert("zplat_clint_sizze".into(), Value::bits(64, 0u64));
        cpu.interpreter.operation_limit = u64::MAX;
        cpu.specialize = std::env::var_os("VMLAB_TCG_SPECIALIZE").is_some();
        if std::env::var_os("VMLAB_TCG_PROFILE").is_some() {
            cpu.interpreter.profile = Some(Default::default());
        }
        cpu.set_pc(RAM_BASE);
        let mut work = Self {
            cpu,
            clint,
            kind: kind.into(),
            events: 0,
            expected: 0,
        };
        work.words(RAM_BASE, &[0x00108093, 0xffdff06f])?;
        match kind {
            "ram" | "ram-after-mmio" | "same-page-data" | "self-modifying" => {}
            "sv39-tlb" | "sv48-tlb" => work.paging(if kind == "sv39-tlb" { 8 } else { 9 })?,
            "m-traps" | "s-traps" => {
                let supervisor = kind == "s-traps";
                let csr = if supervisor { 0x141 } else { 0x341 };
                work.words(RAM_BASE, &[0x00000073, 0xffdff06f])?;
                work.words(
                    HANDLER,
                    &[
                        (csr << 20) | 0x00002273,
                        0x00420213,
                        (csr << 20) | 0x00021073,
                        0x00110113,
                        if supervisor { 0x10200073 } else { 0x30200073 },
                    ],
                )?;
                work.cpu
                    .write_csr(if supervisor { 0x105 } else { 0x305 }, HANDLER)?;
                if supervisor {
                    work.cpu.write_csr(0x3b0, (1u64 << 54) - 1)?;
                    work.cpu.write_csr(0x3a0, 0x1f)?;
                    work.cpu.write_csr(0x302, 1 << 9)?;
                    work.cpu.set_privilege(Privilege::Supervisor);
                }
            }
            "clint-timer" | "clint-software" => {
                work.words(HANDLER, &[0x00110113, 0x30200073])?;
                work.cpu.write_csr(0x305, HANDLER)?;
                work.cpu
                    .write_csr(0x304, 1 << if kind == "clint-timer" { 7 } else { 3 })?;
                let status = work.cpu.read_csr(0x300)?;
                work.cpu.write_csr(0x300, status | 8)?;
            }
            "mmio" => {
                work.words(RAM_BASE, &[0x00013183, 0x00108093, 0xff9ff06f])?;
                work.cpu.set_gpr(2, CLINT + 0xbff8)?;
                work.clint.lock().advance(42);
                work.sync_clint();
            }
            "store-heavy" | "store-heavy-after-mmio" => {
                let mut words = Vec::new();
                for _ in 0..16 {
                    words.extend([0x00108093, 0x00113023]);
                }
                words.push(0xf81ff06f); // jal x0, -128
                work.words(RAM_BASE, &words)?;
                work.cpu.set_gpr(2, RAM_BASE + 0x8000)?;
            }
            "large-code" | "fresh-code" | "cache-pressure" => {
                if kind == "cache-pressure" {
                    work.cpu.set_trace_cache_limit(128_000)?;
                }
                let blocks = if kind == "cache-pressure" { 257 } else { 4097 };
                for block in 0..blocks {
                    work.words(
                        RAM_BASE + block * 16,
                        &[0x00108093, 0x00108093, 0x00108093, 0x00108093],
                    )?;
                }
            }
            _ => return Err(format!("unknown workload: {kind}").into()),
        }
        if matches!(kind, "ram-after-mmio" | "store-heavy-after-mmio") {
            // A PCI configuration write takes the production untracked-DMA path.
            work.cpu
                .interpreter
                .memory
                .write(0x30000000, &0u32.to_le_bytes())?;
        }
        Ok(work)
    }

    fn words(&mut self, address: u64, words: &[u32]) -> Result<()> {
        for (index, word) in words.iter().enumerate() {
            self.cpu
                .interpreter
                .memory
                .write(address + index as u64 * 4, &word.to_le_bytes())?;
        }
        Ok(())
    }

    fn paging(&mut self, mode: u64) -> Result<()> {
        let levels = mode - 5;
        for level in 1..levels {
            let next = RAM_BASE + (level + 1) * 4096;
            let pte = ((next >> 12) << 10) | 1;
            self.cpu
                .interpreter
                .memory
                .write(RAM_BASE + level * 4096, &pte.to_le_bytes())?;
        }
        let leaf = RAM_BASE + levels * 4096;
        let code = ((RAM_BASE >> 12) << 10) | 0xcf;
        self.cpu
            .interpreter
            .memory
            .write(leaf + 4 * 8, &code.to_le_bytes())?;
        for page in 0..128u64 {
            let pte = ((0x80008 + page) << 10) | 0xcf;
            self.cpu
                .interpreter
                .memory
                .write(leaf + (16 + page) * 8, &pte.to_le_bytes())?;
            self.cpu
                .interpreter
                .memory
                .write(RAM_BASE + 0x8000 + page * 4096, &(42 + page).to_le_bytes())?;
        }
        self.words(RAM_BASE, &[0x00013183, 0x00108093, 0xff9ff06f])?;
        self.cpu.write_csr(0x3b0, (1u64 << 54) - 1)?;
        self.cpu.write_csr(0x3a0, 0x1f)?;
        self.cpu.write_csr(0x180, (mode << 60) | 0x80001)?;
        self.cpu.set_privilege(Privilege::Supervisor);
        self.cpu.set_pc(0x4000);
        Ok(())
    }

    fn sync_clint(&mut self) {
        let clint = self.clint.lock();
        let globals = &mut self.cpu.interpreter.globals;
        globals.insert("zmtime".into(), Value::bits(64, clint.time()));
        globals.insert("zmtimecmp".into(), Value::bits(64, clint.compare(0)));
        globals.insert(
            "zmsip".into(),
            Value::bits(32, u32::from(clint.pending(0) & 8 != 0)),
        );
    }

    fn run(&mut self, rounds: u64) -> Result<()> {
        for _ in 0..rounds {
            match self.kind.as_str() {
                "ram" | "ram-after-mmio" => {
                    self.cpu.run(24576)?;
                    self.expected += 12288;
                }
                "same-page-data" | "self-modifying" => {
                    for turn in 0..128u64 {
                        let delta = 1 + turn % 2;
                        if self.kind == "self-modifying" {
                            self.words(RAM_BASE, &[((delta as u32) << 20) | 0x8093])?;
                        } else {
                            self.cpu
                                .interpreter
                                .memory
                                .write(RAM_BASE + 0x800, &turn.to_le_bytes())?;
                        }
                        self.cpu.run(192)?;
                        self.expected += 96
                            * if self.kind == "self-modifying" {
                                delta
                            } else {
                                1
                            };
                        self.events += 1;
                    }
                }
                "sv39-tlb" | "sv48-tlb" => {
                    for page in 0..128 {
                        self.cpu.set_gpr(2, 0x10000 + page * 4096)?;
                        self.cpu.run(192)?;
                        if self.cpu.gpr(3)? != 42 + page || self.cpu.pc()? != 0x4000 {
                            return Err("paged load result mismatch".into());
                        }
                        self.expected += 64;
                        self.events += 1;
                    }
                }
                "m-traps" | "s-traps" => {
                    for _ in 0..128 {
                        self.cpu.run(7)?;
                        self.expected += 1;
                        self.events += 1;
                    }
                }
                "clint-timer" | "clint-software" => {
                    for _ in 0..128 {
                        let timer = self.kind == "clint-timer";
                        let address = CLINT + if timer { 0x4000 } else { 0 };
                        if timer {
                            self.clint.lock().advance(1);
                        }
                        self.cpu.interpreter.memory.write(
                            address,
                            &if timer {
                                0u64.to_le_bytes().to_vec()
                            } else {
                                1u32.to_le_bytes().to_vec()
                            },
                        )?;
                        self.sync_clint();
                        self.cpu.run(1)?;
                        if self.cpu.pc()? != HANDLER {
                            return Err("CLINT interrupt did not enter handler".into());
                        }
                        self.cpu.interpreter.memory.write(
                            address,
                            &if timer {
                                u64::MAX.to_le_bytes().to_vec()
                            } else {
                                0u32.to_le_bytes().to_vec()
                            },
                        )?;
                        self.sync_clint();
                        self.cpu.run(3)?;
                        self.expected += 1;
                        self.events += 1;
                    }
                }
                "mmio" => {
                    self.cpu.run(24576)?;
                    if self.cpu.gpr(3)? != 42 {
                        return Err("CLINT MMIO read mismatch".into());
                    }
                    self.expected += 8192;
                    self.events += 8192;
                }
                "store-heavy" | "store-heavy-after-mmio" => {
                    self.cpu.run(33 * 1024)?;
                    self.expected += 16 * 1024;
                    self.events += 16 * 1024;
                    let mut bytes = [0; 8];
                    self.cpu
                        .interpreter
                        .memory
                        .read(RAM_BASE + 0x8000, &mut bytes)?;
                    if u64::from_le_bytes(bytes) != self.expected || self.cpu.pc()? != RAM_BASE {
                        return Err("store-heavy RAM or PC mismatch".into());
                    }
                }
                "large-code" | "fresh-code" | "cache-pressure" => {
                    let blocks = if self.kind == "cache-pressure" {
                        257
                    } else {
                        4097
                    };
                    for block in 0..blocks {
                        let address = RAM_BASE + block * 16;
                        if self.kind == "fresh-code" {
                            self.words(address, &[0x00108093; 4])?;
                        }
                        self.cpu.set_pc(address);
                        self.cpu.run(4)?;
                        self.expected += 4;
                        self.events += 1;
                    }
                }
                _ => unreachable!(),
            }
        }
        let register = if matches!(
            self.kind.as_str(),
            "m-traps" | "s-traps" | "clint-timer" | "clint-software"
        ) {
            2
        } else {
            1
        };
        if self.cpu.gpr(register)? != self.expected {
            return Err(format!(
                "{} result mismatch: x{register}={} expected {}",
                self.kind,
                self.cpu.gpr(register)?,
                self.expected
            )
            .into());
        }
        Ok(())
    }
}

/// Run a fixed workload without spawning a VM, worker, or wall-clock timer.
pub fn measure(kind: &str, rounds: u64) -> Result<serde_json::Value> {
    if !(1..=1024).contains(&rounds) {
        return Err("rounds must be in 1..=1024".into());
    }
    let init = Instant::now();
    let mut work = Workload::new(kind)?;
    let initialization_seconds = init.elapsed().as_secs_f64();
    let warm = Instant::now();
    if kind != "fresh-code" {
        work.run(2)?;
    }
    let warmup_seconds = warm.elapsed().as_secs_f64();
    let steps = work.cpu.steps;
    let retired = work.cpu.read_csr(0xb02)?;
    let operations = work.cpu.interpreter.operations;
    let native = work.cpu.specialization_stats.hits;
    let recordings = work.cpu.specialization_stats.recordings;
    let evictions = work.cpu.specialization_stats.evictions;
    let events = work.events;
    let start = Instant::now();
    work.run(rounds)?;
    let seconds = start.elapsed().as_secs_f64();
    let measured_operations = work.cpu.interpreter.operations - operations;
    let instructions = work.cpu.read_csr(0xb02)?.wrapping_sub(retired);
    let (cache_units, cache_limit) = work.cpu.trace_cache_usage();
    let result = serde_json::json!({
        "instructions": instructions, "steps": work.cpu.steps - steps, "seconds": seconds,
        "mips": instructions as f64 / seconds / 1e6,
        "native_instructions": work.cpu.specialization_stats.hits - native,
        "ir_operations": measured_operations,
        "recordings": work.cpu.specialization_stats.recordings - recordings,
        "cache_evictions": work.cpu.specialization_stats.evictions - evictions,
        "cache_units": cache_units, "cache_limit": cache_limit,
        "events": work.events - events, "checksum": work.expected,
        "initialization_seconds": initialization_seconds, "warmup_seconds": warmup_seconds,
    });
    eprintln!("specialization={:?}", work.cpu.specialization_stats);
    if let Some(profile) = work.cpu.interpreter.profile {
        let mut functions: Vec<_> = profile.functions.into_iter().collect();
        functions.sort_by_key(|(_, (_, own, _))| std::cmp::Reverse(*own));
        for function in functions.into_iter().take(20) {
            eprintln!("function={function:?}");
        }
    }
    Ok(result)
}
