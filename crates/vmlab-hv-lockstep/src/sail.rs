//! Safe owner of a Sail instance and Rust guest memory.
//!
//! Linux/glibc linker namespaces isolate Sail runtime globals between instances.
use crate::{CpuError, Memory, MemoryWrite, State, StepCpu};
use std::cell::UnsafeCell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::ptr::NonNull;

#[repr(C)]
struct Registers {
    gpr: [u64; 32],
    fpr: [u64; 32],
    pc: u64,
    privilege: u8,
    virtualized: u8,
}

type New = unsafe extern "C" fn(*const c_char, u64, *mut c_void, Read, Write) -> *mut c_void;
type Free = unsafe extern "C" fn(*mut c_void);
type GetError = unsafe extern "C" fn() -> *const c_char;
type AccessState = unsafe extern "C" fn(*mut c_void, *mut Registers, bool) -> i32;
type Csr = unsafe extern "C" fn(*mut c_void, u16, *mut u64, bool) -> i32;
type Step = unsafe extern "C" fn(*mut c_void) -> i32;
type Read = unsafe extern "C" fn(*mut c_void, u64, *mut u8, usize) -> bool;
type Write = unsafe extern "C" fn(*mut c_void, u64, *const u8, usize) -> bool;

struct Library(NonNull<c_void>);

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: handle was returned by dlmopen; all model objects are freed first.
        unsafe {
            libc::dlclose(self.0.as_ptr());
        }
    }
}

impl Library {
    fn open(path: &Path) -> Result<Self, CpuError> {
        use std::os::unix::ffi::OsStrExt;
        let path =
            CString::new(path.as_os_str().as_bytes()).map_err(|e| CpuError(e.to_string()))?;
        // SAFETY: valid NUL-terminated filename. Each instance gets independent globals.
        let handle = unsafe {
            libc::dlmopen(
                libc::LM_ID_NEWLM,
                path.as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            )
        };
        NonNull::new(handle).map(Self).ok_or_else(|| {
            // SAFETY: dlerror returns a thread-local NUL-terminated diagnostic on failure.
            let message = unsafe { CStr::from_ptr(libc::dlerror()) }.to_string_lossy();
            CpuError(format!("loading Sail oracle: {message}"))
        })
    }

    fn symbol(&self, name: &CStr) -> Result<*mut c_void, CpuError> {
        // SAFETY: live library handle and valid symbol string.
        let symbol = unsafe { libc::dlsym(self.0.as_ptr(), name.as_ptr()) };
        if symbol.is_null() {
            Err(CpuError(format!("missing oracle symbol {name:?}")))
        } else {
            Ok(symbol)
        }
    }
}

pub struct Sail {
    model: Option<NonNull<c_void>>,
    // Box keeps the callback address stable when Sail moves.
    memory: Box<UnsafeCell<Memory>>,
    config: CString,
    csr_ids: Vec<u16>,
    new: New,
    free: Free,
    error: GetError,
    access: AccessState,
    csr: Csr,
    step_one: Step,
    _library: Library,
}

/// RV64GC machine/supervisor state and counters compared by this adapter.
pub const CORE_CSRS: &[u16] = &[
    0x001, 0x002, 0x003, 0x100, 0x104, 0x105, 0x106, 0x140, 0x141, 0x142, 0x143, 0x144, 0x180,
    0x300, 0x301, 0x302, 0x303, 0x304, 0x305, 0x306, 0x340, 0x341, 0x342, 0x343, 0x344, 0xb00,
    0xb02,
];

impl Sail {
    pub fn open(library: &Path, config: &Path, memory: Memory) -> Result<Self, CpuError> {
        use std::os::unix::ffi::OsStrExt;
        let library = Library::open(library)?;
        // SAFETY: the adapter exports these exact C ABI signatures. The library
        // owner remains alive until after all function pointers and model uses.
        let (new, free, error, access, csr, step_one) = unsafe {
            (
                std::mem::transmute::<*mut c_void, New>(library.symbol(c"vmlab_oracle_new")?),
                std::mem::transmute::<*mut c_void, Free>(library.symbol(c"vmlab_oracle_free")?),
                std::mem::transmute::<*mut c_void, GetError>(
                    library.symbol(c"vmlab_oracle_error")?,
                ),
                std::mem::transmute::<*mut c_void, AccessState>(
                    library.symbol(c"vmlab_oracle_state")?,
                ),
                std::mem::transmute::<*mut c_void, Csr>(library.symbol(c"vmlab_oracle_csr")?),
                std::mem::transmute::<*mut c_void, Step>(library.symbol(c"vmlab_oracle_step")?),
            )
        };
        let config_name =
            CString::new(config.as_os_str().as_bytes()).map_err(|e| CpuError(e.to_string()))?;
        let mut cpu = Self {
            model: None,
            memory: Box::new(UnsafeCell::new(memory)),
            config: config_name,
            csr_ids: CORE_CSRS.to_vec(),
            new,
            free,
            error,
            access,
            csr,
            step_one,
            _library: library,
        };
        cpu.create(cpu.memory().base)?;
        Ok(cpu)
    }

    fn create(&mut self, pc: u64) -> Result<(), CpuError> {
        if let Some(model) = self.model.take() {
            // SAFETY: unique live model from this library.
            unsafe {
                (self.free)(model.as_ptr());
            }
        }
        let memory = self.memory.get().cast();
        // SAFETY: stable boxed memory, callbacks valid until model destruction.
        self.model =
            NonNull::new(unsafe { (self.new)(self.config.as_ptr(), pc, memory, read, write) });
        if self.model.is_none() {
            return Err(self.last_error());
        }
        self.memory.get_mut().take_writes();
        Ok(())
    }

    fn pointer(&self) -> Result<*mut c_void, CpuError> {
        self.model
            .map(NonNull::as_ptr)
            .ok_or_else(|| CpuError("Sail model is not initialized".into()))
    }

    fn last_error(&self) -> CpuError {
        // SAFETY: adapter returns a live thread-local NUL-terminated error string.
        CpuError(
            unsafe { CStr::from_ptr((self.error)()) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn check(&self, code: i32) -> Result<(), CpuError> {
        if code == 0 {
            Ok(())
        } else {
            Err(self.last_error())
        }
    }
}

impl Drop for Sail {
    fn drop(&mut self) {
        if let Some(model) = self.model.take() {
            // SAFETY: unique live model; memory and library still alive.
            unsafe {
                (self.free)(model.as_ptr());
            }
        }
    }
}

impl StepCpu for Sail {
    fn reset(&mut self, state: &State, memory: &Memory) -> Result<(), CpuError> {
        if state.gpr[0] != 0
            || !matches!(state.privilege, 0 | 1 | 3)
            || (state.privilege == 3 && state.virtualized)
        {
            return Err(CpuError("invalid architectural initial state".into()));
        }
        *self.memory.get_mut() = memory.clone();
        self.memory.get_mut().take_writes();
        self.create(state.pc)?;
        for (&id, &value) in &state.csrs {
            if !self.csr_ids.contains(&id) {
                return Err(CpuError(format!(
                    "CSR {id:#x} not in oracle comparison set"
                )));
            }
            let mut value = value;
            // SAFETY: live model, valid scalar output pointer and configured CSR.
            self.check(unsafe { (self.csr)(self.pointer()?, id, &mut value, true) })?;
        }
        let mut registers = Registers {
            gpr: state.gpr,
            fpr: state.fpr,
            pc: state.pc,
            privilege: state.privilege,
            virtualized: u8::from(state.virtualized),
        };
        // SAFETY: live model and valid C layout register object.
        self.check(unsafe { (self.access)(self.pointer()?, &mut registers, true) })?;
        self.memory.get_mut().take_writes();
        Ok(())
    }

    fn state(&mut self) -> Result<State, CpuError> {
        let mut registers = Registers {
            gpr: [0; 32],
            fpr: [0; 32],
            pc: 0,
            privilege: 0,
            virtualized: 0,
        };
        // SAFETY: live model and valid output object.
        self.check(unsafe { (self.access)(self.pointer()?, &mut registers, false) })?;
        let mut csrs = std::collections::BTreeMap::new();
        for &id in &self.csr_ids {
            let mut value = 0;
            // SAFETY: valid output and configured CSR identifier.
            self.check(unsafe { (self.csr)(self.pointer()?, id, &mut value, false) })?;
            csrs.insert(id, value);
        }
        Ok(State {
            gpr: registers.gpr,
            fpr: registers.fpr,
            pc: registers.pc,
            privilege: registers.privilege,
            virtualized: registers.virtualized != 0,
            csrs,
        })
    }

    fn memory(&self) -> &Memory {
        // SAFETY: callbacks run only inside exclusive reset/step calls. The
        // returned borrow prevents either call until it ends. Sail is !Send/!Sync.
        unsafe { &*self.memory.get() }
    }

    fn step(&mut self) -> Result<Vec<MemoryWrite>, CpuError> {
        // SAFETY: exclusive access prevents simultaneous calls on this model.
        self.check(unsafe { (self.step_one)(self.pointer()?) })?;
        Ok(self.memory.get_mut().take_writes())
    }
}

unsafe extern "C" fn read(context: *mut c_void, address: u64, out: *mut u8, len: usize) -> bool {
    // SAFETY: adapter passes the stable Memory pointer and a writable len-byte buffer.
    let memory = unsafe { &*context.cast::<Memory>() };
    let Ok(bytes) = memory.read(address, len) else {
        return false;
    };
    // SAFETY: bounds checked above; C++ output does not alias Rust memory.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, len);
    }
    true
}

unsafe extern "C" fn write(
    context: *mut c_void,
    address: u64,
    input: *const u8,
    len: usize,
) -> bool {
    // SAFETY: adapter passes stable, exclusively borrowed memory and a len-byte buffer.
    let memory = unsafe { &mut *context.cast::<Memory>() };
    // SAFETY: adapter owns the readable buffer for the duration of this call.
    let bytes = unsafe { std::slice::from_raw_parts(input, len) };
    memory.write(address, bytes).is_ok()
}
