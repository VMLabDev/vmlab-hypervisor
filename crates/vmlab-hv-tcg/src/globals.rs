use crate::Value;
use std::collections::BTreeMap;
use std::ops::Deref;

/// Versioned architectural values. Mutable access conservatively invalidates
/// dependents, including mutations made by platform and lockstep callers.
#[derive(Default)]
pub struct Globals {
    values: BTreeMap<String, Value>,
    slots: BTreeMap<String, usize>,
    versions: Vec<u64>,
    clock: u64,
}

impl Globals {
    fn invalidate(&mut self, name: &str) {
        self.clock = self.clock.checked_add(1).expect("global version exhausted");
        let next = self.versions.len();
        let slot = *self.slots.entry(name.to_owned()).or_insert(next);
        if slot == self.versions.len() {
            self.versions.push(self.clock);
        } else {
            self.versions[slot] = self.clock;
        }
    }

    pub fn insert(&mut self, name: String, value: Value) -> Option<Value> {
        if self.values.get(&name) != Some(&value) {
            self.invalidate(&name);
        }
        self.values.insert(name, value)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        self.invalidate(name);
        self.values.get_mut(name)
    }

    pub(crate) fn slot(&self, name: &str) -> Option<usize> {
        self.slots.get(name).copied()
    }

    pub(crate) fn version(&self, slot: usize) -> u64 {
        self.versions[slot]
    }
}

impl Deref for Globals {
    type Target = BTreeMap<String, Value>;

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}
