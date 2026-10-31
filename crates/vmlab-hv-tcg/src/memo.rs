use crate::{Globals, Value};
use std::collections::BTreeMap;
use vmlab_hv_tcg_ir::Program;

pub(crate) struct Memo {
    dependencies: Vec<String>,
    slots: Vec<usize>,
    versions: Vec<u64>,
    values: BTreeMap<Vec<Value>, Value>,
    identity: Option<u64>,
    pure: bool,
}

impl Memo {
    pub fn analyze(program: &Program) -> BTreeMap<String, Self> {
        vmlab_hv_tcg_ir::effects(program)
            .into_iter()
            .filter(|(name, effect)| {
                name != "zread_CSR" && !effect.external && program.functions[name].code.len() >= 32
            })
            .map(|(name, mut effect)| {
                let pure = effect.writes.is_empty();
                effect.reads.extend(effect.writes.iter().cloned());
                (
                    name,
                    Self {
                        dependencies: effect.reads.into_iter().collect(),
                        slots: Vec::new(),
                        versions: Vec::new(),
                        values: BTreeMap::new(),
                        identity: None,
                        pure,
                    },
                )
            })
            .collect()
    }

    pub fn record_dependencies(&self) -> (&[String], bool) {
        (&self.dependencies, self.pure)
    }

    pub fn lookup(&mut self, globals: &mut Globals, args: &[Value]) -> Option<Value> {
        if self.identity != Some(globals.identity()) {
            self.slots.clear();
            self.versions.clear();
            self.values.clear();
            self.identity = Some(globals.identity());
        }
        if self.slots.len() != self.dependencies.len() {
            self.slots = self
                .dependencies
                .iter()
                .filter_map(|name| globals.slot(name))
                .collect();
            if self.slots.len() != self.dependencies.len() {
                return None;
            }
        }
        if self.versions.len() != self.dependencies.len()
            || self
                .slots
                .iter()
                .zip(&self.versions)
                .any(|(slot, version)| globals.version(*slot) != *version)
        {
            self.values.clear();
            self.versions = self
                .slots
                .iter()
                .map(|slot| globals.version(*slot))
                .collect();
        }
        // Writes participate in the guards, and insertion requires that none
        // changed. Replaying unchanged writes would only copy aggregates.
        self.values.get(args).cloned()
    }

    pub fn insert(&mut self, globals: &Globals, args: Vec<Value>, value: Value) {
        if self.identity != Some(globals.identity()) {
            return;
        }
        // Results are valid for the input state, not an altered post-call state.
        // Read/write dependencies must remain unchanged across the miss.
        if self.versions.len() != self.dependencies.len()
            || self
                .slots
                .iter()
                .zip(&self.versions)
                .any(|(slot, version)| globals.version(*slot) != *version)
        {
            return;
        }
        if self.values.len() == 128 {
            self.values.clear();
        }
        self.values.insert(args, value);
    }
}
