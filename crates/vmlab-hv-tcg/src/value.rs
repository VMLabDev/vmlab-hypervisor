use crate::{Error, Result};
use num_bigint::{BigInt, BigUint, Sign};
use num_traits::ToPrimitive;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    Unit,
    Bool(bool),
    Integer(BigInt),
    Bits { width: u32, value: BigUint },
    String(String),
    Enum(String),
    Struct(BTreeMap<String, Value>),
    Union(String, Box<Value>),
    Vector(Vec<Value>),
    Ref(String),
    Undefined,
}

impl Value {
    pub fn int(value: impl Into<BigInt>) -> Self {
        Self::Integer(value.into())
    }
    pub fn bits(width: u32, value: impl Into<BigUint>) -> Self {
        Self::Bits {
            width,
            value: value.into() & mask(width),
        }
    }
    pub fn boolean(&self) -> Result<bool> {
        match self {
            Self::Bool(b) => Ok(*b),
            _ => Err(Error::Type("expected bool".into())),
        }
    }
    pub fn integer(&self) -> Result<BigInt> {
        match self {
            Self::Integer(n) => Ok(n.clone()),
            Self::Bits { value, .. } => Ok(BigInt::from(value.clone())),
            _ => Err(Error::Type("expected integer".into())),
        }
    }
    pub fn u64(&self) -> Result<u64> {
        self.integer()?
            .to_u64()
            .ok_or_else(|| Error::Type("integer outside u64".into()))
    }
    pub fn index(&self) -> Result<usize> {
        let index = self
            .integer()?
            .to_usize()
            .ok_or_else(|| Error::Type("invalid index".into()))?;
        if index > 1_048_576 {
            return Err(Error::Type("model allocation/index limit exceeded".into()));
        }
        Ok(index)
    }
    pub fn bitvector(&self) -> Result<(u32, &BigUint)> {
        match self {
            Self::Bits { width, value } => Ok((*width, value)),
            _ => Err(Error::Type("expected bitvector".into())),
        }
    }
    pub fn signed(&self) -> Result<BigInt> {
        let (width, value) = self.bitvector()?;
        let unsigned = BigInt::from(value.clone());
        Ok(if width != 0 && value.bit(u64::from(width - 1)) {
            unsigned - (BigInt::from(1) << width)
        } else {
            unsigned
        })
    }
    pub fn field(&self, field: &str) -> Result<&Value> {
        match self {
            Self::Struct(fields) => fields
                .get(field)
                .ok_or_else(|| Error::Type(format!("missing field {field}"))),
            _ => Err(Error::Type(format!("expected struct for field {field}"))),
        }
    }
    pub fn string(&self) -> Result<&str> {
        match self {
            Self::String(s) => Ok(s),
            _ => Err(Error::Type("expected string".into())),
        }
    }
}

pub fn mask(width: u32) -> BigUint {
    if width == 0 {
        BigUint::ZERO
    } else {
        (BigUint::from(1u8) << width) - BigUint::from(1u8)
    }
}

pub fn integer_bits(width: u32, value: BigInt) -> Value {
    let modulus = BigInt::from(1) << width;
    let mut n: BigInt = value % &modulus;
    if n.sign() == Sign::Minus {
        n += modulus;
    }
    Value::bits(width, n.magnitude().clone())
}
