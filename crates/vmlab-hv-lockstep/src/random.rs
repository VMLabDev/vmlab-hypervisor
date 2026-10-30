//! Seeded RV64I streams. Every generated word has a valid base-ISA encoding.

pub fn stream(mut seed: u64, instructions: usize) -> Vec<u8> {
    if seed == 0 {
        seed = 1;
    }
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed as u32
    };
    let mut bytes = Vec::with_capacity(instructions.saturating_mul(4));
    for _ in 0..instructions {
        let n = next();
        let rd = (n >> 3) & 31;
        let rs1 = (n >> 8) & 31;
        let rs2 = (n >> 13) & 31;
        let funct3 = (n >> 18) & 7;
        let word = match n % 5 {
            0 => ((next() & 0xfff) << 20) | (rs1 << 15) | (rd << 7) | 0x13, // ADDI
            1 => (next() & 0xffff_f000) | (rd << 7) | 0x37,                 // LUI
            2 => (next() & 0xffff_f000) | (rd << 7) | 0x17,                 // AUIPC
            3 => (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x33,
            _ => {
                let f3 = if n & 0x100_0000 == 0 { 1 } else { 5 };
                let arithmetic = if f3 == 5 && n & 0x200_0000 != 0 {
                    1 << 30
                } else {
                    0
                };
                arithmetic | ((next() & 63) << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | 0x13
            }
        };
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes
}
