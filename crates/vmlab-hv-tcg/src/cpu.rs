use crate::{Interpreter, Memory, Result, Value};
use std::collections::BTreeMap;
use vmlab_hv_tcg_ir::{Expr, Function, Location, Op, Target, Type};

const TRACE_CACHE_LIMIT: usize = 8_000_000;

#[derive(Debug)]
pub struct TraceProfile {
    pub pc: u64,
    pub calls: u32,
    pub instructions: u32,
    pub operations: usize,
    pub guards: usize,
    pub inputs: usize,
    pub outputs: usize,
}

fn general_names() -> &'static std::collections::BTreeSet<String> {
    static NAMES: std::sync::OnceLock<std::collections::BTreeSet<String>> =
        std::sync::OnceLock::new();
    NAMES.get_or_init(|| (1..32).map(|i| format!("zx{i}")).collect())
}

fn dynamic_names() -> &'static std::collections::BTreeSet<String> {
    static NAMES: std::sync::OnceLock<std::collections::BTreeSet<String>> =
        std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        [
            "znextPC",
            "zmcycle",
            "zminstret",
            "zmtime",
            "zminstret_increment",
            "ztlb",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(general_names().iter().cloned())
        .chain((0..32).map(|i| format!("zf{i}")))
        .collect()
    })
}

fn platform_dynamic_names() -> &'static std::collections::BTreeSet<String> {
    static NAMES: std::sync::OnceLock<std::collections::BTreeSet<String>> =
        std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = dynamic_names().clone();
        // Platform timers, IRQs, and trap handlers change these data values.
        // They must not invalidate otherwise reusable instruction traces.
        names.extend(
            [
                "zmtimecmp",
                "zstimecmp",
                "zmsip",
                "zmip",
                "zmepc",
                "zsepc",
                "zmcause",
                "zscause",
                "zmtval",
                "zstval",
                "zmscratch",
                "zsscratch",
            ]
            .into_iter()
            .map(str::to_owned),
        );
        names
    })
}

#[derive(Default, Debug)]
pub struct SpecializationStats {
    pub recordings: u64,
    pub training_steps: u64,
    pub hits: u64,
    pub trace_runs: u64,
    pub exits: u64,
    pub misses: u64,
    pub scalars: u64,
    pub evictions: u64,
    pub max_trace: Option<(u64, usize, [usize; 4])>,
    pub stop_reasons: BTreeMap<String, u64>,
}

fn step_function() -> &'static Function {
    static FUNCTION: std::sync::OnceLock<Function> = std::sync::OnceLock::new();
    FUNCTION.get_or_init(|| Function {
        name: "__vmlab_step".into(),
        args: vec![("step".into(), Type::Integer(None))],
        result: Type::Bool,
        locals: BTreeMap::new(),
        code: vec![
            Op::Call(
                Location::Id("return".into()),
                Target::Function("ztry_step".into()),
                vec![Expr::Id("step".into()), Expr::Bool(false)],
            ),
            Op::Call(
                Location::Id("tick".into()),
                Target::Function("ztick_clock".into()),
                vec![Expr::Unit],
            ),
            Op::Return,
        ],
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Privilege {
    User,
    Supervisor,
    Machine,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct CpuState {
    globals: BTreeMap<String, Value>,
    steps: u64,
    reservation: Option<(u64, u64)>,
    external_clock: bool,
}

/// One hart owns its architectural state and reservation. RAM sharing belongs
/// to the Memory implementation; no CPU state is stored in process globals.
pub struct Riscv64<M> {
    pub interpreter: Interpreter<'static, M>,
    pub steps: u64,
    pub specialize: bool,
    pub specialization_stats: SpecializationStats,
    traces: BTreeMap<u64, Vec<crate::trace::Trace>>,
    cache_units: usize,
    cache_limit: usize,
    cache_clock: u64,
    external_clock: bool,
}

impl<M: Memory> Riscv64<M> {
    pub fn save_state(&self) -> CpuState {
        CpuState {
            globals: (*self.interpreter.globals).clone(),
            steps: self.steps,
            reservation: self.interpreter.reservation,
            external_clock: self.external_clock,
        }
    }

    /// Restore architecture state into a fresh runtime, discarding translation,
    /// memoization, and source-continuation caches.
    pub fn restore_state(&mut self, state: &CpuState) -> Result<()>
    where
        M: Clone,
    {
        let mut restored = Self::new(self.interpreter.memory.clone())?;
        if state.external_clock != self.external_clock {
            return Err(crate::Error::Type(
                "CPU snapshot uses a different platform clock".into(),
            ));
        }
        if self.external_clock {
            restored.use_external_clock()?;
        }
        if !restored.interpreter.globals.keys().eq(state.globals.keys()) {
            return Err(crate::Error::Type(
                "CPU snapshot has a different Sail model".into(),
            ));
        }
        for (name, value) in &state.globals {
            restored
                .interpreter
                .globals
                .insert(name.clone(), value.clone());
        }
        restored.steps = state.steps;
        restored.interpreter.reservation = state.reservation;
        restored.specialize = self.specialize;
        restored.interpreter.memoize = self.interpreter.memoize;
        restored.interpreter.indexed = self.interpreter.indexed;
        restored.interpreter.operation_limit = self.interpreter.operation_limit;
        restored.cache_limit = self.cache_limit;
        *self = restored;
        Ok(())
    }

    pub fn trace_profile(&self) -> Vec<TraceProfile> {
        let mut profiles: Vec<_> = self
            .traces
            .iter()
            .flat_map(|(pc, traces)| traces.iter().map(|trace| trace.profile(*pc)))
            .collect();
        profiles.sort_by_key(|profile| std::cmp::Reverse(profile.calls));
        profiles
    }

    /// Set a positive weighted metadata budget and discard existing traces.
    /// Units account for recipes, scalar operations, and continuation state;
    /// they are not bytes. Architectural state is unchanged.
    pub fn set_trace_cache_limit(&mut self, units: usize) -> Result<()> {
        if units == 0 {
            return Err(crate::Error::Type(
                "trace cache budget must be positive".into(),
            ));
        }
        self.specialization_stats.evictions += self
            .traces
            .values()
            .map(|variants| variants.len() as u64)
            .sum::<u64>();
        self.traces.clear();
        self.cache_units = 0;
        self.cache_limit = units;
        Ok(())
    }

    /// Current weighted metadata usage and its configured limit.
    pub fn trace_cache_usage(&self) -> (usize, usize) {
        (self.cache_units, self.cache_limit)
    }

    pub fn new(memory: M) -> Result<Self> {
        let mut interpreter = Interpreter::new(vmlab_hv_tcg_riscv64::model(), memory);
        interpreter.initialize()?;
        interpreter.call("zinitializze_registers", vec![Value::Unit])?;
        interpreter.call("zreset", vec![Value::Unit])?;
        Ok(Self {
            interpreter,
            steps: 0,
            specialize: false,
            specialization_stats: SpecializationStats::default(),
            traces: BTreeMap::new(),
            cache_units: 0,
            cache_limit: TRACE_CACHE_LIMIT,
            cache_clock: 0,
            external_clock: false,
        })
    }
    pub fn pc(&self) -> Result<u64> {
        self.interpreter.globals["zPC"].u64()
    }

    /// Let the machine supply MTIME. The generated cycle counter still ticks,
    /// but instruction retirement no longer advances the platform's timer.
    /// Configure this before executing any guest instructions.
    pub fn use_external_clock(&mut self) -> Result<()> {
        if self.steps != 0 {
            return Err(crate::Error::Type(
                "platform clock must be configured before execution".into(),
            ));
        }
        let mut function = self
            .interpreter
            .program
            .functions
            .get("zclint_tick_clock")
            .ok_or_else(|| crate::Error::Undefined("Sail platform clock".into()))?
            .clone();
        let mut replaced = false;
        for operation in &mut function.code {
            if matches!(operation, Op::Assign(Location::Id(name), _) if name == "zmtime") {
                *operation = Op::Assign(Location::Id("return".into()), Expr::Unit);
                replaced = true;
            }
        }
        if !replaced {
            return Err(crate::Error::Type(
                "Sail model has no replaceable MTIME tick".into(),
            ));
        }
        function.name = "__vmlab_platform_tick_clock".into();
        self.interpreter.platform_tick = Some(std::sync::Arc::new(function));
        self.external_clock = true;
        Ok(())
    }
    pub fn set_pc(&mut self, pc: u64) {
        self.interpreter
            .globals
            .insert("zPC".into(), Value::bits(64, pc));
        self.interpreter
            .globals
            .insert("znextPC".into(), Value::bits(64, pc));
    }
    pub fn gpr(&self, register: u8) -> Result<u64> {
        if register == 0 {
            return Ok(0);
        }
        self.interpreter
            .globals
            .get(&format!("zx{register}"))
            .ok_or_else(|| crate::Error::Type("GPR index outside 0..32".into()))?
            .u64()
    }
    pub fn set_gpr(&mut self, register: u8, value: u64) -> Result<()> {
        if register >= 32 {
            return Err(crate::Error::Type("GPR index outside 0..32".into()));
        }
        if register != 0 {
            self.interpreter
                .globals
                .insert(format!("zx{register}"), Value::bits(64, value));
        }
        Ok(())
    }
    pub fn set_privilege(&mut self, privilege: Privilege) {
        self.interpreter.globals.insert(
            "zcur_privilege".into(),
            Value::Enum(
                match privilege {
                    Privilege::User => "zUser",
                    Privilege::Supervisor => "zSupervisor",
                    Privilege::Machine => "zMachine",
                }
                .into(),
            ),
        );
    }
    pub fn read_csr(&mut self, csr: u16) -> Result<u64> {
        self.interpreter
            .call("zread_CSR", vec![Value::bits(12, csr)])?
            .u64()
    }
    pub fn write_csr(&mut self, csr: u16, value: u64) -> Result<()> {
        self.interpreter.call(
            "zwrite_CSR",
            vec![Value::bits(12, csr), Value::bits(64, value)],
        )?;
        Ok(())
    }
    /// Executes fetch, translation, decode, semantics and trap entry from Sail.
    /// Returns true if the hart is waiting rather than advancing its PC.
    pub fn step(&mut self) -> Result<bool> {
        self.interpreter.fetch_address = None;
        if self.specialize {
            let pc_slot = self.interpreter.globals.slot("zPC");
            self.interpreter.globals.begin_native();
            let result = self.specialized_run(1, pc_slot);
            self.interpreter.globals.end_native();
            return result.map(|(_, waiting)| waiting);
        }
        let waiting = self
            .interpreter
            .call(
                "ztry_step",
                vec![Value::int(self.steps), Value::Bool(false)],
            )?
            .boolean()?;
        self.interpreter.call("ztick_clock", vec![Value::Unit])?;
        self.steps += 1;
        Ok(waiting)
    }

    /// Run up to `limit` guest instructions, stopping if the hart waits.
    pub fn run(&mut self, limit: u64) -> Result<(u64, bool)> {
        if self.specialize {
            self.interpreter.globals.begin_native();
        }
        let result = self.run_inner(limit);
        if self.specialize {
            self.interpreter.globals.end_native();
        }
        result
    }

    fn run_inner(&mut self, limit: u64) -> Result<(u64, bool)> {
        let start = self.steps;
        let pc_slot = self
            .specialize
            .then(|| self.interpreter.globals.slot("zPC"))
            .flatten();
        while self.steps - start < limit {
            let waiting = if self.specialize {
                self.specialized_run((limit - (self.steps - start)).min(256) as u32, pc_slot)?
                    .1
            } else {
                self.step()?
            };
            if waiting || self.interpreter.watch_hit {
                return Ok((self.steps - start, waiting));
            }
        }
        Ok((self.steps - start, false))
    }

    /// Stop after the guest instruction that stores across `address`. The
    /// watched byte is not read, so this also preserves MMIO access ordering.
    pub fn run_until_store(&mut self, limit: u64, address: u64) -> Result<(u64, bool)> {
        self.interpreter.watch_store = Some(address);
        self.interpreter.watch_hit = false;
        let result = self.run(limit);
        self.interpreter.watch_store = None;
        self.interpreter.watch_hit = false;
        result
    }

    fn specialized_run(&mut self, limit: u32, pc_slot: Option<usize>) -> Result<(u32, bool)> {
        let pc = match pc_slot.and_then(|slot| self.interpreter.globals.native_slot(slot)) {
            Some((vmlab_hv_tcg_ir::native::ScalarType::Bits(64), raw)) => raw as u64,
            Some(_) => return Err(crate::Error::Type("native PC is not Bits(64)".into())),
            None => self.pc()?,
        };
        self.cache_clock = self
            .cache_clock
            .checked_add(1)
            .expect("trace cache clock exhausted");
        if let Some(trace) = self
            .traces
            .get_mut(&pc)
            .and_then(|variants| variants.last_mut())
            && !trace.promote(limit)
        {
            let before = trace.cache_units();
            let outcome = trace.run_step(&mut self.interpreter, self.steps, limit);
            trace.last_used = self.cache_clock;
            let after = trace.cache_units();
            if before != after {
                let variants = self.traces.remove(&pc).expect("cached variants");
                self.cache_units -= variants
                    .iter()
                    .map(crate::trace::Trace::cache_units)
                    .sum::<usize>()
                    - after
                    + before;
                self.cache_variants(pc, variants);
            }
            if !matches!(outcome, Ok(crate::trace::Outcome::Miss)) {
                return self
                    .account_outcome(outcome)
                    .map(|result| result.expect("matched trace"));
            }
        }
        let args = vec![Value::int(self.steps)];
        let mut variants = self.traces.remove(&pc).unwrap_or_default();
        let mut unbounded_globals = std::collections::BTreeSet::new();
        for trace in &variants {
            unbounded_globals.extend(trace.unbounded_globals().iter().cloned());
            unbounded_globals.extend(trace.failed_ranges(&self.interpreter));
        }
        self.cache_units -= variants
            .iter()
            .map(crate::trace::Trace::cache_units)
            .sum::<usize>();
        let mut promotion = None;
        for index in (0..variants.len()).rev() {
            if variants[index].promote(limit)
                && variants[index].matches_entry(&self.interpreter, &args)
            {
                promotion = Some(index);
                break;
            }
            let outcome = variants[index].run(&mut self.interpreter, &args, limit);
            if !matches!(outcome, Ok(crate::trace::Outcome::Miss)) {
                let mut matched = variants.remove(index);
                matched.last_used = self.cache_clock;
                variants.push(matched);
                self.cache_variants(pc, std::mem::take(&mut variants));
            }
            if let Some(result) = self.account_outcome(outcome)? {
                return Ok(result);
            }
        }
        let training_limit = if let Some(index) = promotion {
            variants[index].promotion_limit(limit)
        } else {
            limit.min(32)
        };
        let names = if self.external_clock {
            platform_dynamic_names()
        } else {
            dynamic_names()
        };
        let dynamic_globals = self
            .interpreter
            .globals
            .keys()
            .filter(|name| name.starts_with("zrvfi_") || names.contains(name.as_str()))
            .cloned()
            .collect();
        let options = crate::record::Options {
            dynamic_globals,
            pc: Some(pc),
            step_argument: Some(0),
            narrow_ram_inputs: true,
            stop_at_control_flow: promotion.is_none(),
            unbounded_globals,
            small_integer_globals: general_names().clone(),
        };
        let recording = crate::trace::Trace::record_block(
            &mut self.interpreter,
            step_function(),
            args,
            options,
            training_limit,
        );
        if recording.is_err() {
            self.steps += u64::from(self.interpreter.trace_completed);
        }
        let (mut trace, value, steps) = recording?;
        if std::env::var("VMLAB_TCG_TRACE_PC")
            .ok()
            .is_some_and(|value| {
                value == "all"
                    || u64::from_str_radix(value.trim_start_matches("0x"), 16).ok() == Some(pc)
            })
        {
            trace.dump();
        }
        self.specialization_stats.recordings += 1;
        self.specialization_stats.training_steps += u64::from(steps);
        self.specialization_stats.scalars += trace.scalar_count() as u64;
        if let Some(reason) = trace.stop_reason() {
            *self
                .specialization_stats
                .stop_reasons
                .entry(reason.into())
                .or_default() += 1;
        }
        trace.last_used = self.cache_clock;
        if let Some(index) = promotion {
            variants.remove(index);
        }
        variants.push(trace);
        self.cache_variants(pc, variants);
        self.steps += u64::from(steps);
        Ok((steps, value.boolean()?))
    }

    fn cache_variants(&mut self, pc: u64, mut variants: Vec<crate::trace::Trace>) {
        for trace in &variants {
            if self
                .specialization_stats
                .max_trace
                .as_ref()
                .is_none_or(|(_, size, _)| *size < trace.cache_units())
            {
                self.specialization_stats.max_trace =
                    Some((pc, trace.cache_units(), trace.cache_breakdown()));
            }
        }
        variants.retain(|trace| trace.cache_units() <= self.cache_limit);
        if variants.is_empty() {
            return;
        }
        let mut cache_units: usize = variants.iter().map(crate::trace::Trace::cache_units).sum();
        while cache_units > self.cache_limit {
            cache_units -= variants.remove(0).cache_units();
            self.specialization_stats.evictions += 1;
        }
        while cache_units <= self.cache_limit
            && !self.traces.is_empty()
            && self.cache_units + cache_units > self.cache_limit
        {
            let oldest = *self
                .traces
                .iter()
                .min_by_key(|(_, variants)| {
                    variants.last().map(|trace| trace.last_used).unwrap_or(0)
                })
                .expect("nonempty cache")
                .0;
            let evicted = self.traces.remove(&oldest).expect("cached trace");
            self.cache_units -= evicted
                .iter()
                .map(crate::trace::Trace::cache_units)
                .sum::<usize>();
            self.specialization_stats.evictions += evicted.len() as u64;
        }
        if !variants.is_empty() {
            self.cache_units += cache_units;
            self.traces.insert(pc, variants);
        }
    }

    fn account_outcome(
        &mut self,
        outcome: Result<crate::trace::Outcome>,
    ) -> Result<Option<(u32, bool)>> {
        if outcome.is_err() {
            self.steps += u64::from(self.interpreter.trace_completed);
        }
        match outcome? {
            crate::trace::Outcome::Returned(value, steps) => {
                self.specialization_stats.trace_runs += 1;
                self.specialization_stats.hits += u64::from(steps);
                self.steps += u64::from(steps);
                Ok(Some((steps, value.boolean()?)))
            }
            crate::trace::Outcome::Resumed(value, steps) => {
                self.specialization_stats.trace_runs += 1;
                self.specialization_stats.hits += u64::from(steps - 1);
                self.specialization_stats.exits += 1;
                self.steps += u64::from(steps);
                Ok(Some((steps, value.boolean()?)))
            }
            crate::trace::Outcome::Miss => {
                self.specialization_stats.misses += 1;
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FlatMemory;
    use std::collections::BTreeMap;
    use vmlab_hv_tcg_ir::{Location, Program, Type};

    fn cache_cpu(limit: usize) -> Riscv64<FlatMemory> {
        static PROGRAM: std::sync::OnceLock<Program> = std::sync::OnceLock::new();
        let program = PROGRAM.get_or_init(|| {
            let step = Function {
                name: "ztry_step".into(),
                args: vec![
                    ("step".into(), Type::Integer(None)),
                    ("trace".into(), Type::Bool),
                ],
                result: Type::Bool,
                locals: BTreeMap::new(),
                code: vec![
                    Op::Call(
                        Location::Id("zPC".into()),
                        Target::Intrinsic("add_bits_int".into()),
                        vec![Expr::Id("zPC".into()), Expr::Integer("4".into(), None)],
                    ),
                    Op::Assign(Location::Id("return".into()), Expr::Bool(false)),
                    Op::Return,
                ],
            };
            let tick = Function {
                name: "ztick_clock".into(),
                args: vec![("unit".into(), Type::Unit)],
                result: Type::Unit,
                locals: BTreeMap::new(),
                code: vec![
                    Op::Assign(Location::Id("return".into()), Expr::Unit),
                    Op::Return,
                ],
            };
            let mut program = Program::default();
            program.globals.insert("zPC".into(), Type::Bits(Some(64)));
            program.functions.insert(step.name.clone(), step);
            program.functions.insert(tick.name.clone(), tick);
            program
        });
        let mut interpreter = Interpreter::new(program, FlatMemory::new(0, vec![]));
        interpreter.initialize().unwrap();
        Riscv64 {
            interpreter,
            steps: 0,
            specialize: true,
            specialization_stats: SpecializationStats::default(),
            traces: BTreeMap::new(),
            cache_units: 0,
            cache_limit: limit,
            cache_clock: 0,
            external_clock: false,
        }
    }

    #[test]
    fn pc_working_set_that_fits_the_budget_stays_native() {
        let mut cpu = cache_cpu(TRACE_CACHE_LIMIT);
        for pc in (0..4097).map(|index| index * 4) {
            cpu.set_pc(pc);
            cpu.step().unwrap();
            assert_eq!(cpu.pc().unwrap(), pc + 4);
        }
        assert!(cpu.cache_units < cpu.cache_limit);
        eprintln!(
            "working set: {} PCs, {}/{} cache units, {} evictions",
            cpu.traces.len(),
            cpu.cache_units,
            cpu.cache_limit,
            cpu.specialization_stats.evictions
        );
        assert_eq!(cpu.specialization_stats.evictions, 0);
        let recordings = cpu.specialization_stats.recordings;
        let operations = cpu.interpreter.operations;
        for pc in (0..4097).map(|index| index * 4) {
            cpu.set_pc(pc);
            cpu.step().unwrap();
            assert_eq!(cpu.pc().unwrap(), pc + 4);
            assert_eq!(cpu.interpreter.operations, operations);
            assert_eq!(cpu.specialization_stats.recordings, recordings);
        }
    }

    #[test]
    fn pc_working_set_exceeding_the_budget_evicts_and_stays_bounded() {
        let mut cpu = cache_cpu(512);
        for pc in (0..64).map(|index| index * 4) {
            cpu.set_pc(pc);
            cpu.step().unwrap();
            assert!(cpu.cache_units <= cpu.cache_limit);
        }
        assert!(cpu.specialization_stats.evictions > 0);
        assert!(cpu.traces.len() < 64);
        let operations = cpu.interpreter.operations;
        cpu.set_pc(63 * 4);
        cpu.step().unwrap();
        assert_eq!(cpu.pc().unwrap(), 64 * 4);
        assert_eq!(cpu.interpreter.operations, operations);
    }
}
