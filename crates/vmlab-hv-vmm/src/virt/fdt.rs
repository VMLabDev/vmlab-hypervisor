//! Flattened device tree for the captured virt memory map, with ACPI disabled.

use std::collections::BTreeMap;

use super::{ISA, RAM_BASE};

#[derive(Default)]
struct Tree {
    structure: Vec<u8>,
    strings: Vec<u8>,
    names: BTreeMap<String, u32>,
}

impl Tree {
    fn word(&mut self, value: u32) {
        self.structure.extend_from_slice(&value.to_be_bytes());
    }
    fn align(&mut self) {
        while !self.structure.len().is_multiple_of(4) {
            self.structure.push(0);
        }
    }
    fn begin(&mut self, name: &str) {
        self.word(1);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.align();
    }
    fn end(&mut self) {
        self.word(2);
    }
    fn property(&mut self, name: &str, data: &[u8]) {
        let offset = *self.names.entry(name.to_owned()).or_insert_with(|| {
            let offset = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            offset
        });
        self.word(3);
        self.word(data.len() as u32);
        self.word(offset);
        self.structure.extend_from_slice(data);
        self.align();
    }
    fn cells(&mut self, name: &str, values: &[u32]) {
        self.property(
            name,
            &values
                .iter()
                .flat_map(|v| v.to_be_bytes())
                .collect::<Vec<_>>(),
        );
    }
    fn text(&mut self, name: &str, value: &str) {
        let mut data = value.as_bytes().to_vec();
        data.push(0);
        self.property(name, &data);
    }
    fn reg(&mut self, base: u64, len: u64) {
        self.cells(
            "reg",
            &[
                (base >> 32) as u32,
                base as u32,
                (len >> 32) as u32,
                len as u32,
            ],
        );
    }
    fn finish(mut self) -> Vec<u8> {
        self.word(9);
        let structure_offset = 56u32;
        let strings_offset = structure_offset + self.structure.len() as u32;
        let total = strings_offset + self.strings.len() as u32;
        let mut blob = Vec::new();
        for value in [
            0xd00d_feed,
            total,
            structure_offset,
            strings_offset,
            40,
            17,
            16,
            0,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ] {
            blob.extend_from_slice(&value.to_be_bytes());
        }
        blob.extend_from_slice(&[0; 16]);
        blob.extend(self.structure);
        blob.extend(self.strings);
        blob
    }
}

pub fn generate(
    harts: u32,
    ram_size: u64,
    cmdline: &str,
    initrd: Option<(u64, u64)>,
    seed: Option<&[u8; 32]>,
) -> Vec<u8> {
    let mut t = Tree::default();
    t.begin("");
    t.cells("#address-cells", &[2]);
    t.cells("#size-cells", &[2]);
    t.text("compatible", "riscv-virtio");
    t.text("model", "riscv-virtio,qemu");
    for (name, value) in [("poweroff", 0x5555), ("reboot", 0x7777)] {
        t.begin(name);
        t.text("compatible", &format!("syscon-{name}"));
        t.cells("regmap", &[2 * harts + 2]);
        t.cells("offset", &[0]);
        t.cells("value", &[value]);
        t.end();
    }
    t.begin("memory@80000000");
    t.text("device_type", "memory");
    t.reg(RAM_BASE, ram_size);
    t.end();
    t.begin("cpus");
    t.cells("#address-cells", &[1]);
    t.cells("#size-cells", &[0]);
    t.cells("timebase-frequency", &[10_000_000]);
    for hart in 0..harts {
        t.begin(&format!("cpu@{hart:x}"));
        t.cells("phandle", &[2 * hart + 1]);
        t.text("device_type", "cpu");
        t.cells("reg", &[hart]);
        t.text("status", "okay");
        t.text("compatible", "riscv");
        t.text("riscv,isa", ISA);
        t.text("riscv,isa-base", "rv64i");
        t.text("riscv,isa-extensions", "i\0m\0a\0f\0d\0c\0zicsr\0zifencei");
        t.text("mmu-type", "riscv,sv57");
        t.begin("interrupt-controller");
        t.cells("#interrupt-cells", &[1]);
        t.property("interrupt-controller", &[]);
        t.text("compatible", "riscv,cpu-intc");
        t.cells("phandle", &[2 * hart + 2]);
        t.end();
        t.end();
    }
    t.begin("cpu-map");
    t.begin("cluster0");
    for hart in 0..harts {
        t.begin(&format!("core{hart}"));
        t.cells("cpu", &[2 * hart + 1]);
        t.end();
    }
    t.end();
    t.end();
    t.end();
    t.begin("pmu");
    t.text("compatible", "riscv,pmu");
    t.cells(
        "riscv,event-to-mhpmcounters",
        &[
            1, 1, 0x7fff9, 2, 2, 0x7fffc, 0x10019, 0x10019, 0x7fff8, 0x1001b, 0x1001b, 0x7fff8,
            0x10021, 0x10021, 0x7fff8,
        ],
    );
    t.end();
    t.begin("fw-cfg@10100000");
    t.text("compatible", "qemu,fw-cfg-mmio");
    t.reg(0x1010_0000, 0x18);
    t.property("dma-coherent", &[]);
    t.end();
    t.begin("flash@20000000");
    t.text("compatible", "cfi-flash");
    t.cells("bank-width", &[4]);
    t.cells(
        "reg",
        &[0, 0x2000_0000, 0, 0x200_0000, 0, 0x2200_0000, 0, 0x200_0000],
    );
    t.end();
    t.begin("aliases");
    t.text("serial0", "/soc/serial@10000000");
    t.end();
    t.begin("chosen");
    t.text("stdout-path", "/soc/serial@10000000");
    if let Some(seed) = seed {
        t.property("rng-seed", seed);
    }
    if !cmdline.is_empty() {
        t.text("bootargs", cmdline);
    }
    if let Some((start, end)) = initrd {
        t.cells("linux,initrd-start", &[(start >> 32) as u32, start as u32]);
        t.cells("linux,initrd-end", &[(end >> 32) as u32, end as u32]);
    }
    t.end();
    let plic = 2 * harts + 1;
    t.begin("platform-bus@4000000");
    t.text("compatible", "qemu,platform\0simple-bus");
    t.cells("#address-cells", &[1]);
    t.cells("#size-cells", &[1]);
    t.cells("ranges", &[0, 0, 0x400_0000, 0x200_0000]);
    t.cells("interrupt-parent", &[plic]);
    t.end();
    t.begin("soc");
    t.cells("#address-cells", &[2]);
    t.cells("#size-cells", &[2]);
    t.text("compatible", "simple-bus");
    t.property("ranges", &[]);
    for (name, compatible, base, len, irq) in [
        ("rtc@101000", "google,goldfish-rtc", 0x10_1000, 0x1000, 11),
        ("serial@10000000", "ns16550a", 0x1000_0000, 0x100, 10),
        (
            "test@100000",
            "sifive,test1\0sifive,test0\0syscon",
            0x10_0000,
            0x1000,
            0,
        ),
    ] {
        t.begin(name);
        t.text("compatible", compatible);
        t.reg(base, len);
        if irq != 0 {
            t.cells("interrupt-parent", &[plic]);
            t.cells("interrupts", &[irq]);
        } else {
            t.cells("phandle", &[plic + 1]);
        }
        if irq == 10 {
            t.cells("clock-frequency", &[3_686_400]);
        }
        t.end();
    }
    for slot in (1..=8).rev() {
        t.begin(&format!("virtio_mmio@{:x}", 0x1000_0000 + slot * 0x1000));
        t.text("compatible", "virtio,mmio");
        t.reg(0x1000_0000 + slot as u64 * 0x1000, 0x1000);
        t.cells("interrupt-parent", &[plic]);
        t.cells("interrupts", &[slot]);
        t.end();
    }
    t.begin("interrupt-controller@c000000");
    t.cells("phandle", &[plic]);
    t.cells("riscv,ndev", &[95]);
    t.reg(0xc00_0000, 0x60_0000);
    let external: Vec<_> = (0..harts)
        .flat_map(|hart| [2 * hart + 2, 11, 2 * hart + 2, 9])
        .collect();
    t.cells("interrupts-extended", &external);
    t.property("interrupt-controller", &[]);
    t.text("compatible", "sifive,plic-1.0.0\0riscv,plic0");
    t.cells("#address-cells", &[0]);
    t.cells("#interrupt-cells", &[1]);
    t.end();
    t.begin("clint@2000000");
    let local: Vec<_> = (0..harts)
        .flat_map(|hart| [2 * hart + 2, 3, 2 * hart + 2, 7])
        .collect();
    t.cells("interrupts-extended", &local);
    t.reg(0x200_0000, 0x1_0000);
    t.text("compatible", "sifive,clint0\0riscv,clint0");
    t.end();
    t.begin("pci@30000000");
    t.text("compatible", "pci-host-ecam-generic");
    t.text("device_type", "pci");
    t.cells("#address-cells", &[3]);
    t.cells("#size-cells", &[2]);
    t.cells("#interrupt-cells", &[1]);
    t.cells("linux,pci-domain", &[0]);
    t.cells("bus-range", &[0, 255]);
    t.property("dma-coherent", &[]);
    t.reg(0x3000_0000, 0x1000_0000);
    t.cells(
        "ranges",
        &[
            0x1000000, 0, 0, 0, 0x3000000, 0, 0x10000, 0x2000000, 0, 0x40000000, 0, 0x40000000, 0,
            0x40000000, 0x3000000, 4, 0, 4, 0, 4, 0,
        ],
    );
    t.cells("interrupt-map-mask", &[0x1800, 0, 0, 7]);
    let map: Vec<_> = (0..4)
        .flat_map(|slot| {
            (1..=4).flat_map(move |pin| [slot << 11, 0, 0, pin, plic, 32 + (slot + pin - 1) % 4])
        })
        .collect();
    t.cells("interrupt-map", &map);
    t.end();
    t.end();
    t.end();
    t.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dtc_accepts_tree_for_single_and_multiple_harts() {
        for harts in [1, 2, 4] {
            let blob = generate(
                harts,
                2 << 30,
                "console=ttyS0",
                Some((0x90000000, 0x91000000)),
                Some(&[0; 32]),
            );
            if let Some(directory) = std::env::var_os("VMLAB_HV_RISCV_FDT_ARTIFACTS") {
                let path = std::path::PathBuf::from(directory).join(format!("virt-{harts}.dtb"));
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .unwrap();
                file.write_all(&generate(harts, 2 << 30, "", None, None))
                    .unwrap();
            }
            assert_eq!(
                u32::from_be_bytes(blob[4..8].try_into().unwrap()) as usize,
                blob.len()
            );
            let mut child = std::process::Command::new("dtc")
                .args(["-I", "dtb", "-O", "dts"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("dtc is required for device-tree validation");
            use std::io::Write;
            child.stdin.take().unwrap().write_all(&blob).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let dts = String::from_utf8(output.stdout).unwrap();
            assert!(dts.contains(ISA));
            assert!(dts.contains("riscv,ndev = <0x5f>"));
            assert!(dts.contains(&format!("core{}", harts - 1)));
        }
    }
}
