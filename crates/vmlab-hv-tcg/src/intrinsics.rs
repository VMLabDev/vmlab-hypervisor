use crate::value::{integer_bits, mask};
use crate::{Error, Interpreter, Memory, Result, Value};
use num_bigint::{BigInt, BigUint, Sign};
use num_traits::{Signed, ToPrimitive, Zero};
use vmlab_hv_tcg_ir::NativeOperation;

impl<M: Memory> Interpreter<'_, M> {
    pub(crate) fn native(
        &mut self,
        operation: &NativeOperation,
        width: u32,
        args: &[Value],
    ) -> Result<Value> {
        let first = args
            .first()
            .ok_or_else(|| Error::Type("native operation arity".into()))?;
        let x = first
            .bitvector()?
            .1
            .to_u128()
            .ok_or_else(|| Error::Type("native operand wider than u128".into()))?;
        let mask = if width == 128 {
            u128::MAX
        } else {
            (1u128 << width) - 1
        };
        let operand = || -> Result<u128> {
            let value = args
                .get(1)
                .ok_or_else(|| Error::Type("native binary operation arity".into()))?;
            // Signed integer additions reduce modulo the bit width.
            let value = integer_bits(128, value.integer()?);
            value
                .bitvector()?
                .1
                .to_u128()
                .ok_or_else(|| Error::Type("native operand wider than u128".into()))
        };
        let result = match operation {
            NativeOperation::Add => x.wrapping_add(operand()?),
            NativeOperation::Sub => x.wrapping_sub(operand()?),
            NativeOperation::Not => !x,
            NativeOperation::And => x & operand()?,
            NativeOperation::Or => x | operand()?,
            NativeOperation::Xor => x ^ operand()?,
            NativeOperation::ShiftLeft => {
                let shift = operand()?;
                if shift >= u128::from(width) {
                    0
                } else {
                    x << shift
                }
            }
            NativeOperation::ShiftRight => {
                let shift = operand()?;
                if shift >= u128::from(width) {
                    0
                } else {
                    x >> shift
                }
            }
            NativeOperation::ArithmeticShiftRight => {
                if width == 0 {
                    0
                } else {
                    let signed = ((x << (128 - width)) as i128) >> (128 - width);
                    (signed >> operand()?.min(u128::from(width - 1))) as u128
                }
            }
        };
        Ok(Value::bits(width, result & mask))
    }

    pub(crate) fn intrinsic(
        &mut self,
        name: &str,
        width: Option<u32>,
        args: &[Value],
    ) -> Result<Value> {
        let a = |i: usize| {
            args.get(i)
                .ok_or_else(|| Error::Type(format!("{name}: missing arg {i}")))
        };
        let int = |i| a(i)?.integer();
        let index = |i| a(i)?.index();
        let bits = |i| a(i)?.bitvector();
        let bool_ = |i| a(i)?.boolean();
        let result = match name {
            "%i->%i64" => {
                let v = int(0)?;
                let n = v
                    .to_i64()
                    .ok_or_else(|| Error::Type("Sail integer does not fit i64".into()))?;
                Value::int(n)
            }
            "%i64->%i" | "monomorphize" => a(0)?.clone(),
            "@not" | "not" => Value::Bool(!bool_(0)?),
            "@and" | "and_bool" => Value::Bool(bool_(0)? && bool_(1)?),
            "@or" | "or_bool" => Value::Bool(bool_(0)? || bool_(1)?),
            "@eq" | "eq_int" | "eq_bool" | "eq_bits" | "eq_string" | "eq_anything" => {
                Value::Bool(a(0)? == a(1)?)
            }
            "@neq" | "neq_bits" => Value::Bool(a(0)? != a(1)?),
            "@lt" | "lt" => Value::Bool(int(0)? < int(1)?),
            "@lteq" | "lteq" => Value::Bool(int(0)? <= int(1)?),
            "@gt" | "gt" => Value::Bool(int(0)? > int(1)?),
            "@gteq" | "gteq" => Value::Bool(int(0)? >= int(1)?),
            "@iadd" | "add_int" => Value::int(int(0)? + int(1)?),
            "@isub" | "sub_int" => Value::int(int(0)? - int(1)?),
            "mult_int" => Value::int(int(0)? * int(1)?),
            "neg_int" => Value::int(-int(0)?),
            "abs_int" => Value::int(int(0)?.abs()),
            "min_int" => Value::int(int(0)?.min(int(1)?)),
            "max_int" => Value::int(int(0)?.max(int(1)?)),
            "tdiv_int" | "tmod_int" | "ediv_int" => {
                let (x, y) = (int(0)?, int(1)?);
                if y.is_zero() {
                    return Err(Error::Type("division by zero".into()));
                }
                let mut q = &x / &y;
                if name == "ediv_int" && (&x % &y).sign() == Sign::Minus {
                    q -= if y.sign() == Sign::Minus { -1 } else { 1 };
                }
                Value::int(if name == "tmod_int" { x % y } else { q })
            }
            "pow2" => Value::int(BigInt::from(1) << index(0)?),
            "zeros" | "undefined_bitvector" => Value::bits(index(0)? as u32, 0u8),
            "undefined_bool" => Value::Bool(false),
            "undefined_vector" => Value::Vector(vec![a(1)?.clone(); index(0)?]),
            "@unsigned" | "sail_unsigned" => Value::int(BigInt::from(bits(0)?.1.clone())),
            "@signed" | "sail_signed" => Value::int(a(0)?.signed()?),
            "@bvnot" | "not_bits" => {
                let (w, x) = bits(0)?;
                Value::bits(w, mask(w) ^ x)
            }
            "@bvor" | "or_bits" => {
                let (w, x) = bits(0)?;
                Value::bits(w, x | bits(1)?.1)
            }
            "@bvand" | "and_bits" => {
                let (w, x) = bits(0)?;
                Value::bits(w, x & bits(1)?.1)
            }
            "@bvxor" | "xor_bits" => {
                let (w, x) = bits(0)?;
                Value::bits(w, x ^ bits(1)?.1)
            }
            "@bvadd" | "add_bits" | "add_bits_int" => {
                let (w, x) = bits(0)?;
                integer_bits(w, BigInt::from(x.clone()) + int(1)?)
            }
            "@bvsub" | "sub_bits" | "sub_bits_int" => {
                let (w, x) = bits(0)?;
                integer_bits(w, BigInt::from(x.clone()) - int(1)?)
            }
            "@concat" | "append" | "append_64" => {
                let (w, x) = bits(0)?;
                let (v, y) = bits(1)?;
                Value::bits(w + v, (x << v) | y)
            }
            "length" => Value::int(match a(0)? {
                Value::Bits { width, .. } => *width as usize,
                Value::Vector(v) => v.len(),
                _ => return Err(Error::Type("length type".into())),
            }),
            "vector_init" => Value::Vector(vec![a(1)?.clone(); index(0)?]),
            "zinternal_vector_init" => Value::Vector(vec![Value::Undefined; index(0)?]),
            "vector_access" | "@bvaccess" => {
                let i = index(1)?;
                match a(0)? {
                    Value::Vector(v) => v
                        .get(i)
                        .cloned()
                        .ok_or_else(|| Error::Type("vector index".into()))?,
                    Value::Bits { width, value } if i < *width as usize => {
                        Value::bits(1, u8::from(value.bit(i as u64)))
                    }
                    _ => return Err(Error::Type("vector access".into())),
                }
            }
            "vector_update" | "zinternal_vector_update" => {
                let i = index(1)?;
                match a(0)?.clone() {
                    Value::Vector(mut v) => {
                        *v.get_mut(i)
                            .ok_or_else(|| Error::Type("vector index".into()))? = a(2)?.clone();
                        Value::Vector(v)
                    }
                    Value::Bits { width, mut value } if i < width as usize => {
                        value.set_bit(i as u64, !bits(2)?.1.is_zero());
                        Value::bits(width, value)
                    }
                    _ => return Err(Error::Type("vector update".into())),
                }
            }
            "vector_subrange" => {
                let (_, x) = bits(0)?;
                let (hi, lo) = (index(1)?, index(2)?);
                if hi < lo {
                    return Err(Error::Type("reversed subrange".into()));
                }
                Value::bits((hi - lo + 1) as u32, x >> lo)
            }
            "vector_update_subrange" => {
                let (w, x) = bits(0)?;
                let (hi, lo) = (index(1)?, index(2)?);
                if hi < lo || hi >= w as usize {
                    return Err(Error::Type("subrange outside vector".into()));
                }
                let m = mask((hi - lo + 1) as u32) << lo;
                Value::bits(w, (x & (mask(w) ^ &m)) | ((bits(3)?.1 << lo) & m))
            }
            "@slice" => {
                let (_, x) = bits(0)?;
                Value::bits(
                    width.ok_or_else(|| Error::Type("slice width".into()))?,
                    x >> index(1)?,
                )
            }
            "get_slice_int" => integer_bits(index(0)? as u32, int(1)? >> index(2)?),
            "sign_extend" => integer_bits(index(1)? as u32, a(0)?.signed()?),
            "zero_extend" | "sail_truncate" => Value::bits(index(1)? as u32, bits(0)?.1.clone()),
            "@zero_extend" => Value::bits(
                width.ok_or_else(|| Error::Type("zero extend width".into()))?,
                bits(0)?.1.clone(),
            ),
            "shiftl" | "shift_bits_left" => {
                let (w, x) = bits(0)?;
                let n = int(1)?;
                if n >= BigInt::from(w) {
                    Value::bits(w, 0u8)
                } else {
                    Value::bits(w, x << index(1)?)
                }
            }
            "shiftr" | "shift_bits_right" => {
                let (w, x) = bits(0)?;
                let n = int(1)?;
                if n >= BigInt::from(w) {
                    Value::bits(w, 0u8)
                } else {
                    Value::bits(w, x >> index(1)?)
                }
            }
            "arith_shiftr" => {
                let (w, _) = bits(0)?;
                let shift = index(1)?.min(w as usize);
                integer_bits(w, a(0)?.signed()? >> shift)
            }
            "count_leading_zeros" => {
                let (w, x) = bits(0)?;
                Value::int(u64::from(w) - x.bits())
            }
            "count_trailing_zeros" => {
                let (w, x) = bits(0)?;
                Value::int(x.trailing_zeros().unwrap_or(u64::from(w)))
            }
            "@hd" => match a(0)? {
                Value::Vector(v) => v
                    .first()
                    .cloned()
                    .ok_or_else(|| Error::Type("empty head".into()))?,
                _ => return Err(Error::Type("list head".into())),
            },
            "@tl" => match a(0)? {
                Value::Vector(v) if !v.is_empty() => Value::Vector(v[1..].to_vec()),
                _ => return Err(Error::Type("list tail".into())),
            },
            "@is_empty" => match a(0)? {
                Value::Vector(v) => Value::Bool(v.is_empty()),
                _ => return Err(Error::Type("list empty".into())),
            },
            "cons" => match a(1)? {
                Value::Vector(v) => {
                    let mut out = vec![a(0)?.clone()];
                    out.extend_from_slice(v);
                    Value::Vector(out)
                }
                _ => return Err(Error::Type("cons list".into())),
            },
            "zsail_assert" => {
                if !bool_(0)? {
                    return Err(Error::Exception(a(1)?.string()?.to_owned()));
                }
                Value::Unit
            }
            "zupdate_fbits" => {
                let (w, mut x) = (bits(0)?.0, bits(0)?.1.clone());
                x.set_bit(index(1)? as u64, !bits(2)?.1.is_zero());
                Value::bits(w, x)
            }
            "concat_str" => Value::String(format!("{}{}", a(0)?.string()?, a(1)?.string()?)),
            "dec_str" => Value::String(int(0)?.to_string()),
            "hex_str" => Value::String(format!("{:x}", int(0)?)),
            "string_of_bits" => Value::String(format!("0x{:x}", bits(0)?.1)),
            "print" | "print_endline" | "print_int" | "print_bits" | "print_string" => {
                let text = args
                    .iter()
                    .map(|value| match value {
                        Value::String(s) => s.clone(),
                        Value::Integer(n) => n.to_string(),
                        Value::Bits { value, .. } => format!("0x{value:x}"),
                        other => format!("{other:?}"),
                    })
                    .collect::<String>();
                if self.diagnostics.len() == 4096 {
                    self.diagnostics.pop_front();
                }
                self.diagnostics.push_back(text);
                Value::Unit
            }
            "sail_barrier" => {
                std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
                Value::Unit
            }
            // These are optional Sail observation hooks, not architectural ops.
            "instr_announce" | "branch_announce" | "event_callback" => Value::Unit,
            "reset_registers" => self.call("zinitializze_registers", vec![Value::Unit])?,
            "load_reservation" => {
                // read_mem_exclusive captured the generation with the read. Do
                // not refresh it after another hart could have invalidated it.
                if !self.reservation.is_some_and(|(address, _)| {
                    a(0).and_then(Value::u64)
                        .is_ok_and(|expected| address == expected)
                }) {
                    return Err(Error::Type(
                        "reservation callback without exclusive read".into(),
                    ));
                }
                Value::Unit
            }
            "match_reservation" => {
                Value::Bool(self.reservation == Some((a(0)?.u64()?, self.memory.generation())))
            }
            "cancel_reservation" => {
                self.reservation = None;
                Value::Unit
            }
            "valid_reservation" => Value::Bool(
                self.reservation
                    .is_some_and(|(_, g)| g == self.memory.generation()),
            ),
            "sys_enable_experimental_extensions" => Value::Bool(false),
            "parse_hex_bits" => {
                let s = a(1)?.string()?.trim_start_matches("0x");
                let n = BigUint::parse_bytes(s.as_bytes(), 16)
                    .ok_or_else(|| Error::Type("invalid hex string".into()))?;
                Value::bits(index(0)? as u32, n)
            }
            "valid_hex_bits" => Value::Bool(
                BigUint::parse_bytes(a(1)?.string()?.trim_start_matches("0x").as_bytes(), 16)
                    .is_some_and(|n| n.bits() <= index(0).unwrap_or(0) as u64),
            ),
            _ => return Err(Error::Intrinsic(name.into())),
        };
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FlatMemory;
    use vmlab_hv_tcg_ir::Program;

    #[test]
    fn native_and_generic_bitvector_operations_agree_at_width_boundaries() {
        let program = Program::default();
        let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
        let operations = [
            (NativeOperation::Add, "add_bits"),
            (NativeOperation::Sub, "sub_bits"),
            (NativeOperation::And, "and_bits"),
            (NativeOperation::Or, "or_bits"),
            (NativeOperation::Xor, "xor_bits"),
            (NativeOperation::ShiftLeft, "shiftl"),
            (NativeOperation::ShiftRight, "shiftr"),
            (NativeOperation::ArithmeticShiftRight, "arith_shiftr"),
        ];
        for width in [0, 1, 8, 16, 33, 64, 127, 128] {
            for value in [0, 1, u64::MAX as u128, u128::MAX] {
                for (op, generic) in &operations {
                    for second in [0, 1, 7, 64, 127, 128, 255] {
                        let args = vec![
                            Value::bits(width, value),
                            Value::bits(width, second as u128),
                        ];
                        assert_eq!(
                            interpreter.native(op, width, &args).unwrap(),
                            interpreter.intrinsic(generic, None, &args).unwrap(),
                            "{op:?} width {width}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn wide_packet_concat_retains_more_than_128_bits() {
        let program = Program::default();
        let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
        let result = interpreter
            .intrinsic(
                "append",
                None,
                &[Value::bits(128, u128::MAX), Value::bits(64, u64::MAX)],
            )
            .unwrap();
        let (width, bits) = result.bitvector().unwrap();
        assert_eq!(width, 192);
        assert_eq!(bits.bits(), 192);
    }
}
