use crate::Value;
use num_traits::ToPrimitive;
use std::collections::BTreeMap;
use std::ops::Deref;
use vmlab_hv_tcg_ir::native::ScalarType;

/// Versioned architectural values. Mutable access conservatively invalidates
/// dependents, including mutations made by platform and lockstep callers.
pub struct Globals {
    values: BTreeMap<String, Value>,
    slots: BTreeMap<String, usize>,
    versions: Vec<u64>,
    clock: u64,
    identity: u64,
    native_active: bool,
    native: Vec<Option<(ScalarType, u128)>>,
    native_dirty: Vec<usize>,
    names: Vec<String>,
    context_watched: Vec<bool>,
    context_clock: u64,
}

impl Default for Globals {
    fn default() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let identity = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_ne!(identity, 0, "global identity exhausted");
        Self {
            values: BTreeMap::new(),
            slots: BTreeMap::new(),
            versions: Vec::new(),
            clock: 0,
            identity,
            native_active: false,
            native: Vec::new(),
            native_dirty: Vec::new(),
            names: Vec::new(),
            context_watched: Vec::new(),
            context_clock: 0,
        }
    }
}

impl Globals {
    fn invalidate(&mut self, name: &str) {
        self.clock = self.clock.checked_add(1).expect("global version exhausted");
        let slot = if let Some(slot) = self.slots.get(name) {
            *slot
        } else {
            let slot = self.versions.len();
            self.slots.insert(name.to_owned(), slot);
            slot
        };
        if slot == self.versions.len() {
            self.versions.push(self.clock);
            self.native.push(None);
            self.names.push(name.to_owned());
            self.context_watched.push(false);
        } else {
            self.versions[slot] = self.clock;
            if self.context_watched[slot] {
                self.context_clock = self.clock;
            }
        }
    }

    pub fn insert(&mut self, name: String, value: Value) -> Option<Value> {
        self.flush_native_name(&name);
        if self.values.get(&name) != Some(&value) {
            self.invalidate(&name);
        }
        self.values.insert(name, value)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        self.flush_native_name(name);
        self.invalidate(name);
        self.values.get_mut(name)
    }

    pub(crate) fn slot(&self, name: &str) -> Option<usize> {
        self.slots.get(name).copied()
    }

    pub(crate) fn version(&self, slot: usize) -> u64 {
        self.versions[slot]
    }

    #[cfg(test)]
    pub(crate) fn revision(&self) -> u64 {
        self.clock
    }

    pub(crate) fn identity(&self) -> u64 {
        self.identity
    }

    pub(crate) fn begin_native(&mut self) {
        assert!(!self.native_active);
        self.native_active = true;
    }

    pub(crate) fn end_native(&mut self) {
        self.flush_native();
        self.native_active = false;
    }

    pub(crate) fn flush_native(&mut self) {
        for slot in self.native_dirty.drain(..) {
            let Some((ty, raw)) = self.native[slot].take() else {
                continue;
            };
            let value = match ty {
                ScalarType::Bool => Value::Bool(raw != 0),
                ScalarType::Bits(width) => Value::bits(width.into(), raw),
                ScalarType::Integer => Value::int(raw as i128),
            };
            *self
                .values
                .get_mut(&self.names[slot])
                .expect("native global") = value;
        }
    }

    fn flush_native_name(&mut self, name: &str) {
        let Some((ty, raw)) = self.slot(name).and_then(|slot| self.native[slot].take()) else {
            return;
        };
        let value = match ty {
            ScalarType::Bool => Value::Bool(raw != 0),
            ScalarType::Bits(width) => Value::bits(width.into(), raw),
            ScalarType::Integer => Value::int(raw as i128),
        };
        *self.values.get_mut(name).expect("native global") = value;
    }

    pub(crate) fn native_word(&self, name: &str) -> Option<(ScalarType, u128)> {
        self.slot(name).and_then(|slot| self.native[slot])
    }

    pub(crate) fn native_slot(&self, slot: usize) -> Option<(ScalarType, u128)> {
        self.native[slot]
    }

    pub(crate) fn native_value_mut(&mut self, slot: usize) -> &mut Value {
        if let Some((ty, raw)) = self.native[slot].take() {
            let value = match ty {
                ScalarType::Bool => Value::Bool(raw != 0),
                ScalarType::Bits(width) => Value::bits(width.into(), raw),
                ScalarType::Integer => Value::int(raw as i128),
            };
            *self
                .values
                .get_mut(&self.names[slot])
                .expect("native global") = value;
        }
        self.clock = self.clock.checked_add(1).expect("global version exhausted");
        self.versions[slot] = self.clock;
        if self.context_watched[slot] {
            self.context_clock = self.clock;
        }
        self.values
            .get_mut(&self.names[slot])
            .expect("native global")
    }

    #[cfg(test)]
    pub(crate) fn write_native_word(&mut self, name: &str, ty: ScalarType, raw: u128) -> bool {
        if !self.native_active {
            return false;
        }
        let Some(slot) = self.slot(name) else {
            return false;
        };
        self.write_native_slot(slot, ty, raw)
    }

    pub(crate) fn write_native_slot(&mut self, slot: usize, ty: ScalarType, mut raw: u128) -> bool {
        if !self.native_active {
            return false;
        }
        if let ScalarType::Bits(width) = ty {
            raw &= vmlab_hv_tcg_ir::native::mask(width);
        }
        if ty == ScalarType::Bool {
            raw = u128::from(raw != 0);
        }
        if self.native[slot] != Some((ty, raw)) {
            if self.native[slot].is_none() {
                self.native_dirty.push(slot);
            }
            self.native[slot] = Some((ty, raw));
            self.clock = self.clock.checked_add(1).expect("global version exhausted");
            self.versions[slot] = self.clock;
            if self.context_watched[slot] {
                self.context_clock = self.clock;
            }
        }
        true
    }

    pub(crate) fn watch_context(&mut self, slot: usize) {
        self.context_watched[slot] = true;
    }

    pub(crate) fn context_revision(&self) -> u64 {
        self.context_clock
    }

    pub(crate) fn native_matches(&self, name: &str, expected: &Value) -> bool {
        match self.native_word(name) {
            Some((ScalarType::Bool, raw)) => {
                matches!(expected, Value::Bool(value) if *value == (raw != 0))
            }
            Some((ScalarType::Bits(width), raw)) => {
                matches!(expected, Value::Bits { width: actual, value } if *actual == u32::from(width) && value.to_u128() == Some(raw))
            }
            Some((ScalarType::Integer, raw)) => {
                matches!(expected, Value::Integer(value) if value.to_i128() == Some(raw as i128))
            }
            None => self.values.get(name) == Some(expected),
        }
    }
}

impl Deref for Globals {
    type Target = BTreeMap<String, Value>;

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_words_publish_at_boundaries_and_legacy_writes_supersede_them() {
        let mut globals = Globals::default();
        globals.insert("a".into(), Value::bits(64, 1u64));
        globals.insert("b".into(), Value::int(2));
        let slot = globals.slot("a").unwrap();
        let version = globals.version(slot);
        globals.begin_native();
        assert!(globals.write_native_word("a", ScalarType::Bits(64), 3));
        assert!(globals.version(slot) > version);
        assert!(globals.native_matches("a", &Value::bits(64, 3u64)));
        assert!(!globals.native_matches("a", &Value::bits(32, 3u64)));
        globals.insert("b".into(), Value::int(4));
        assert_eq!(globals.native_word("a"), Some((ScalarType::Bits(64), 3)));
        globals.insert("a".into(), Value::bits(64, 5u64));
        assert_eq!(globals.native_word("a"), None);
        assert!(globals.write_native_word("a", ScalarType::Bits(64), 6));
        assert!(globals.write_native_word("b", ScalarType::Integer, (-7i128) as u128));
        globals.end_native();
        assert_eq!(globals["a"], Value::bits(64, 6u64));
        assert_eq!(globals["b"], Value::int(-7));
        assert_eq!(globals.native_word("a"), None);
        assert!(!globals.write_native_word("a", ScalarType::Bits(64), 8));
    }
}
