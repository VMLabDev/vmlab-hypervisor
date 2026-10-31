use vmlab_hv_tcg::{Memory, Riscv64, Value};

use super::RAM_BASE;

/// Replace the standalone Sail platform's PMAs with virt's physical map.
/// Keep the model's complete RAM and IO attribute records, including its
/// misalignment and atomicity policy; only flash permits IO instruction fetch.
pub(super) fn configure_memory<M: Memory>(cpu: &mut Riscv64<M>, size: u64) -> Result<(), String> {
    let Value::Vector(defaults) = &cpu.interpreter.globals["zpma_regions"] else {
        return Err("Sail PMA regions are not a list".into());
    };
    let io = defaults
        .iter()
        .find(|r| r.field("zbase").and_then(Value::u64).ok() == Some(0x2000000))
        .ok_or("Sail IO PMA is missing")?
        .clone();
    let ram = defaults
        .iter()
        .find(|r| r.field("zbase").and_then(Value::u64).ok() == Some(RAM_BASE))
        .ok_or("Sail RAM PMA is missing")?
        .clone();
    let mut regions = Vec::new();
    for (base, length, executable, main) in [
        (0x100000, 0x2000, false, false),
        (0x2000000, 0x10000, false, false),
        (0x3000000, 0x10000, false, false),
        (0xc000000, 0x600000, false, false),
        (0x10000000, 0x1000, false, false),
        (0x10100000, 0x1000, false, false),
        (0x20000000, 0x4000000, true, false),
        (0x30000000, 0x50000000, false, false),
        (RAM_BASE, size, true, true),
        (0x4_00000000, 0x4_00000000, false, false),
    ] {
        let Value::Struct(mut region) = (if main { &ram } else { &io }).clone() else {
            return Err("Sail PMA region is not a struct".into());
        };
        region.insert("zbase".into(), Value::bits(64, base));
        region.insert("zsizze".into(), Value::bits(64, length));
        let Some(Value::Struct(attributes)) = region.get_mut("zattributes") else {
            return Err("Sail PMA attributes are missing".into());
        };
        attributes.insert("zexecutable".into(), Value::Bool(executable));
        if executable && !main {
            attributes.insert("zread_idempotent".into(), Value::Bool(true));
        }
        regions.push(Value::Struct(region));
    }
    cpu.interpreter
        .globals
        .insert("zpma_regions".into(), Value::Vector(regions));
    // Captured QEMU virt identity and the sixteen implemented HPM counters.
    cpu.interpreter
        .globals
        .insert("zmarchid".into(), Value::bits(64, 0x2au64));
    cpu.interpreter.globals.insert(
        "zsys_writable_hpm_counters".into(),
        Value::bits(32, 0x0007fff8u32),
    );
    cpu.interpreter
        .globals
        .insert("znum_triggers".into(), Value::int(2));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmlab_hv_tcg::{FlatMemory, SharedMemory};

    #[test]
    fn flash_fetch_requires_virt_pmas_and_survives_cpu_restore() {
        let mut memory = SharedMemory::new(FlatMemory::new(0x20000000, vec![0; 4096]));
        memory
            .write(0x20000000, &0x00700513u32.to_le_bytes())
            .unwrap();
        let mut cpu = Riscv64::new(memory).unwrap();
        cpu.set_pc(0x20000000);
        cpu.run(1).unwrap();
        assert_eq!(cpu.gpr(10).unwrap(), 0);
        assert_eq!(cpu.read_csr(0x342).unwrap(), 1);
        configure_memory(&mut cpu, 2 << 30).unwrap();
        assert_eq!(cpu.read_csr(0xf12).unwrap(), 0x2a);
        cpu.write_csr(0xb12, 17).unwrap();
        assert_eq!(cpu.read_csr(0xb12).unwrap(), 17);
        cpu.write_csr(0xb13, 17).unwrap();
        assert_eq!(cpu.read_csr(0xb13).unwrap(), 0);
        cpu.write_csr(0x7a0, 1).unwrap();
        assert_eq!(cpu.read_csr(0x7a0).unwrap(), 1);
        cpu.write_csr(0x7a0, u64::MAX).unwrap();
        assert_eq!(cpu.read_csr(0x7a0).unwrap(), 1);
        cpu.set_pc(0x20000000);
        let state = cpu.save_state();
        cpu.run(1).unwrap();
        assert_eq!(cpu.gpr(10).unwrap(), 7);
        cpu.restore_state(&state).unwrap();
        assert_eq!(cpu.gpr(10).unwrap(), 0);
        cpu.run(1).unwrap();
        assert_eq!(cpu.gpr(10).unwrap(), 7);
    }
}
