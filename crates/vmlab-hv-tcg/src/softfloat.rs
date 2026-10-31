use crate::{Error, Result, Value};
use std::{collections::BTreeMap, ffi::CString, sync::Mutex};
use vmlab_hv_tcg_ir::{Program, Type};

static SOFTFLOAT: Mutex<()> = Mutex::new(());

unsafe extern "C" {
    fn hv_softfloat(
        name: *const std::ffi::c_char,
        p0: u64,
        p1: u64,
        p2: u64,
        p3: u64,
        flags: *mut u8,
    ) -> u64;
}

pub fn execute(program: &Program, name: &str, args: &[Value]) -> Result<Value> {
    let (types, result) = program
        .declarations
        .get(name)
        .ok_or_else(|| Error::Intrinsic(name.into()))?;
    if types.len() != args.len() || args.len() > 4 {
        return Err(Error::Type("SoftFloat argument count".into()));
    }
    let Type::Struct(tuple) = result else {
        return Err(Error::Type("SoftFloat result tuple".into()));
    };
    let fields = program
        .structs
        .get(tuple)
        .ok_or_else(|| Error::Type("SoftFloat result fields".into()))?;
    if fields.len() != 2 {
        return Err(Error::Type("SoftFloat result arity".into()));
    }
    let mut inputs = [0u64; 4];
    for (i, value) in args.iter().enumerate() {
        inputs[i] = match value {
            Value::Bool(b) => u64::from(*b),
            _ => value.u64()?,
        };
    }
    let c_name = CString::new(name).map_err(|_| Error::Type("NUL in SoftFloat name".into()))?;
    let _guard = SOFTFLOAT
        .lock()
        .map_err(|_| Error::Type("poisoned SoftFloat lock".into()))?;
    let mut flags = 0;
    // SAFETY: c_name is NUL-terminated and alive through the call; flags points
    // to a writable byte. The mutex serializes SoftFloat's process-global state.
    let value = unsafe {
        hv_softfloat(
            c_name.as_ptr(),
            inputs[0],
            inputs[1],
            inputs[2],
            inputs[3],
            &mut flags,
        )
    };
    if flags == 255 {
        return Err(Error::Intrinsic(name.into()));
    }
    let mut output = BTreeMap::new();
    output.insert(fields[0].0.clone(), Value::bits(5, flags));
    output.insert(
        fields[1].0.clone(),
        match fields[1].1 {
            Type::Bool => Value::Bool(value != 0),
            Type::Bits(Some(w)) => Value::bits(w, value),
            _ => return Err(Error::Type("SoftFloat result type".into())),
        },
    );
    Ok(Value::Struct(output))
}
