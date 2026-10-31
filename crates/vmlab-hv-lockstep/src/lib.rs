//! Per-instruction differential execution. No KVM or VMM dependencies.

pub mod corpus;
pub mod random;
#[cfg(feature = "sail")]
pub mod sail;

use std::collections::BTreeMap;
use std::fmt::{self, Write};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub gpr: [u64; 32],
    /// Raw IEEE bits, including NaN boxing.
    pub fpr: [u64; 32],
    pub pc: u64,
    /// Architectural privilege encoding: U=0, S=1, M=3.
    pub privilege: u8,
    pub virtualized: bool,
    /// Both CPUs must expose the same CSR set, including unchanged CSRs.
    pub csrs: BTreeMap<u16, u64>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            gpr: [0; 32],
            fpr: [0; 32],
            pc: 0x8000_0000,
            privilege: 3,
            virtualized: false,
            csrs: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryWrite {
    pub address: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memory {
    pub base: u64,
    bytes: Vec<u8>,
    writes: Vec<MemoryWrite>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CpuError(pub String);

impl Memory {
    pub fn new(base: u64, bytes: Vec<u8>) -> Result<Self, CpuError> {
        base.checked_add(bytes.len() as u64)
            .ok_or_else(|| CpuError("memory address overflow".into()))?;
        Ok(Self {
            base,
            bytes,
            writes: Vec::new(),
        })
    }

    fn range(&self, address: u64, len: usize) -> Result<std::ops::Range<usize>, CpuError> {
        let start = address
            .checked_sub(self.base)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| CpuError(format!("unmapped memory {address:#x}")))?;
        let end = start
            .checked_add(len)
            .filter(|&n| n <= self.bytes.len())
            .ok_or_else(|| CpuError(format!("unmapped memory {address:#x}+{len}")))?;
        Ok(start..end)
    }

    pub fn read(&self, address: u64, len: usize) -> Result<&[u8], CpuError> {
        Ok(&self.bytes[self.range(address, len)?])
    }

    pub fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), CpuError> {
        let range = self.range(address, bytes.len())?;
        self.bytes[range].copy_from_slice(bytes);
        self.writes.push(MemoryWrite {
            address,
            bytes: bytes.to_vec(),
        });
        Ok(())
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn take_writes(&mut self) -> Vec<MemoryWrite> {
        std::mem::take(&mut self.writes)
    }
}

/// Implementations own independent memory and must drain all writes from one step.
/// `reset` replaces state and memory, clears reservations, timers and write logs.
pub trait StepCpu {
    fn reset(&mut self, state: &State, memory: &Memory) -> Result<(), CpuError>;
    fn state(&mut self) -> Result<State, CpuError>;
    fn memory(&self) -> &Memory;
    fn step(&mut self) -> Result<Vec<MemoryWrite>, CpuError>;
}

#[derive(Debug)]
pub struct Divergence {
    /// Zero denotes an initial-state mismatch, otherwise one-based instruction count.
    pub step: u64,
    pub pc: u64,
    pub instruction: Vec<u8>,
    pub differences: Vec<String>,
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "divergence at step {} pc={:#018x} instruction=",
            self.step, self.pc
        )?;
        for byte in self.instruction.iter().rev() {
            write!(f, "{byte:02x}")?;
        }
        for line in &self.differences {
            write!(f, "\n  {line}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Divergence {}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("{side} failed at step {step}: {source}")]
    Cpu {
        side: &'static str,
        step: u64,
        source: CpuError,
    },
    #[error("{0}")]
    Divergence(Box<Divergence>),
}

fn differences(left: &State, right: &State) -> Vec<String> {
    let mut result = Vec::new();
    for (prefix, a, b) in [("x", &left.gpr, &right.gpr), ("f", &left.fpr, &right.fpr)] {
        for i in 0..32 {
            if a[i] != b[i] {
                result.push(format!(
                    "{prefix}{i}: left={:#018x} right={:#018x}",
                    a[i], b[i]
                ));
            }
        }
    }
    if left.pc != right.pc {
        result.push(format!("pc: left={:#x} right={:#x}", left.pc, right.pc));
    }
    if (left.privilege, left.virtualized) != (right.privilege, right.virtualized) {
        result.push(format!(
            "privilege: left={}/{} right={}/{}",
            left.privilege, left.virtualized, right.privilege, right.virtualized
        ));
    }
    let keys: std::collections::BTreeSet<_> = left.csrs.keys().chain(right.csrs.keys()).collect();
    for key in keys {
        if left.csrs.get(key) != right.csrs.get(key) {
            result.push(format!(
                "csr {key:#05x}: left={:x?} right={:x?}",
                left.csrs.get(key),
                right.csrs.get(key)
            ));
        }
    }
    result
}

fn instruction(cpu: &impl StepCpu, pc: u64) -> Vec<u8> {
    let Ok(first) = cpu.memory().read(pc, 2) else {
        return Vec::new();
    };
    let len = if first[0] & 3 == 3 { 4 } else { 2 };
    cpu.memory().read(pc, len).unwrap_or(first).to_vec()
}

/// Reset both CPUs from exactly the same inputs and stop at the first mismatch.
pub fn run(
    left: &mut impl StepCpu,
    right: &mut impl StepCpu,
    initial: &State,
    memory: &Memory,
    steps: u64,
) -> Result<(), RunError> {
    run_until(left, right, initial, memory, steps, |_| Ok(false)).map(|_| ())
}

#[derive(Debug, PartialEq, Eq)]
pub struct RunOutcome {
    pub steps: u64,
    pub halted: bool,
}

/// The stop predicate observes matched memory after each matched instruction.
pub fn run_until(
    left: &mut impl StepCpu,
    right: &mut impl StepCpu,
    initial: &State,
    memory: &Memory,
    steps: u64,
    mut stop: impl FnMut(&Memory) -> Result<bool, CpuError>,
) -> Result<RunOutcome, RunError> {
    for (side, cpu) in [
        ("left", &mut *left as &mut dyn StepCpu),
        ("right", &mut *right as &mut dyn StepCpu),
    ] {
        cpu.reset(initial, memory).map_err(|source| RunError::Cpu {
            side,
            step: 0,
            source,
        })?;
    }
    for step in 0..=steps {
        let a = left.state().map_err(|source| RunError::Cpu {
            side: "left",
            step,
            source,
        })?;
        let b = right.state().map_err(|source| RunError::Cpu {
            side: "right",
            step,
            source,
        })?;
        let mut diff = differences(&a, &b);
        if step == 0 {
            diff.extend(
                differences(initial, &a)
                    .into_iter()
                    .map(|line| format!("left reset: {line}")),
            );
            diff.extend(
                differences(initial, &b)
                    .into_iter()
                    .map(|line| format!("right reset: {line}")),
            );
        }
        if step == 0
            && (left.memory().base != memory.base
                || right.memory().base != memory.base
                || left.memory().bytes != memory.bytes
                || right.memory().bytes != memory.bytes)
        {
            diff.push("initial memory differs".into());
        }
        if !diff.is_empty() {
            return Err(RunError::Divergence(Box::new(Divergence {
                step,
                pc: a.pc,
                instruction: instruction(left, a.pc),
                differences: diff,
            })));
        }
        if step == steps {
            return Ok(RunOutcome {
                steps,
                halted: false,
            });
        }
        let insn = instruction(left, a.pc);
        let wa = left.step().map_err(|source| RunError::Cpu {
            side: "left",
            step: step + 1,
            source,
        })?;
        let wb = right.step().map_err(|source| RunError::Cpu {
            side: "right",
            step: step + 1,
            source,
        })?;
        let after_a = left.state().map_err(|source| RunError::Cpu {
            side: "left",
            step: step + 1,
            source,
        })?;
        let after_b = right.state().map_err(|source| RunError::Cpu {
            side: "right",
            step: step + 1,
            source,
        })?;
        let mut diff = differences(&after_a, &after_b);
        if wa != wb {
            let mut line = String::new();
            write!(&mut line, "memory writes: left={wa:x?} right={wb:x?}").expect("String write");
            diff.push(line);
        }
        if !diff.is_empty() {
            return Err(RunError::Divergence(Box::new(Divergence {
                step: step + 1,
                pc: a.pc,
                instruction: insn,
                differences: diff,
            })));
        }
        if stop(left.memory()).map_err(|source| RunError::Cpu {
            side: "stop predicate",
            step: step + 1,
            source,
        })? {
            return Ok(RunOutcome {
                steps: step + 1,
                halted: true,
            });
        }
    }
    Ok(RunOutcome {
        steps,
        halted: false,
    })
}
