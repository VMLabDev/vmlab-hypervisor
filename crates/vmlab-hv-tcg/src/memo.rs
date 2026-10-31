use crate::{Globals, Value};
use std::collections::BTreeMap;
use vmlab_hv_tcg_ir::Program;

pub(crate) struct Memo {
    dependencies: Vec<String>,
    slots: Vec<usize>,
    versions: Vec<u64>,
    writes: Vec<String>,
    values: BTreeMap<Vec<Value>, Cached>,
}

struct Cached {
    result: Value,
    writes: Vec<(String, Value)>,
}

impl Memo {
    pub fn analyze(program: &Program) -> BTreeMap<String, Self> {
        vmlab_hv_tcg_ir::effects(program)
            .into_iter()
            .filter(|(name, effect)| {
                name != "zread_CSR" && !effect.external && program.functions[name].code.len() >= 32
            })
            .map(|(name, mut effect)| {
                effect.reads.extend(effect.writes.iter().cloned());
                (
                    name,
                    Self {
                        dependencies: effect.reads.into_iter().collect(),
                        slots: Vec::new(),
                        versions: Vec::new(),
                        writes: effect.writes.into_iter().collect(),
                        values: BTreeMap::new(),
                    },
                )
            })
            .collect()
    }

    pub fn lookup(&mut self, globals: &mut Globals, args: &[Value]) -> Option<Value> {
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
        let cached = self.values.get(args)?;
        for (name, value) in &cached.writes {
            globals.insert(name.clone(), value.clone());
        }
        Some(cached.result.clone())
    }

    pub fn insert(&mut self, globals: &Globals, args: Vec<Value>, value: Value) {
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
        let writes = self
            .writes
            .iter()
            .filter_map(|name| globals.get(name).map(|v| (name.clone(), v.clone())))
            .collect();
        self.values.insert(
            args,
            Cached {
                result: value,
                writes,
            },
        );
    }
}
