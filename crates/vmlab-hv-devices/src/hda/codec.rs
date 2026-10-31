//! Duplex codec, specified by Intel HDA 1.0a chapter 7.

pub(super) const CODEC_BYTES: usize = 6 * 24;

#[derive(Clone)]
pub(super) struct Codec {
    // Per node: input L/R, output L/R, connection, power, stream/channel,
    // pin control, unsolicited enable/tag, EAPD, format, configuration default.
    pub bytes: [u8; CODEC_BYTES],
}

impl Default for Codec {
    fn default() -> Self {
        let mut codec = Self {
            bytes: [0; CODEC_BYTES],
        };
        for node in 1..6 {
            codec.bytes[node * 24..node * 24 + 4].fill(0x80 | 0x4a);
            codec.bytes[node * 24 + 9] = 2;
            codec.bytes[node * 24 + 10..node * 24 + 12].copy_from_slice(&0x11u16.to_le_bytes());
        }
        codec.bytes[3 * 24 + 7] = 0x40;
        codec.bytes[5 * 24 + 7] = 0x20;
        codec.bytes[3 * 24 + 12..3 * 24 + 16].copy_from_slice(&0x0000_4010u32.to_le_bytes());
        codec.bytes[5 * 24 + 12..5 * 24 + 16].copy_from_slice(&0x0080_5020u32.to_le_bytes());
        codec
    }
}

impl Codec {
    pub fn verb(&mut self, command: u32) -> Option<u32> {
        if command >> 28 != 0 || command & (1 << 27) != 0 {
            return None;
        }
        let node = ((command >> 20) & 0x7f) as usize;
        if node >= 6 {
            return Some(0);
        }
        let verb = (command >> 8) & 0xfff;
        let payload = command as u8;
        let wide = command as u16;
        if verb == 0xf00 {
            return Some(Self::parameter(node, payload));
        }
        if node == 0 {
            return Some(0);
        }
        let s = &mut self.bytes[node * 24..(node + 1) * 24];
        let value = match command >> 16 & 0xf {
            0x3 => {
                let offset = if wide & 0x8000 != 0 { 2 } else { 0 };
                if wide & 0x2000 != 0 {
                    s[offset] = payload;
                }
                if wide & 0x1000 != 0 {
                    s[offset + 1] = payload;
                }
                // A payload can address both input and output amplifiers.
                if wide & 0xc000 == 0xc000 {
                    if wide & 0x2000 != 0 {
                        s[0] = payload;
                    }
                    if wide & 0x1000 != 0 {
                        s[1] = payload;
                    }
                }
                0
            }
            0xb => {
                let offset = if wide & 0x8000 != 0 { 2 } else { 0 };
                s[offset + usize::from(wide & 0x2000 == 0)] as u32
            }
            0x2 => {
                s[10..12].copy_from_slice(&wide.to_le_bytes());
                0
            }
            0xa => u16::from_le_bytes([s[10], s[11]]) as u32,
            _ => match verb {
                0xf01 => s[4] as u32,
                0x701 => {
                    s[4] = payload;
                    0
                }
                0xf02 => match (node, payload) {
                    (3, 0) => 2,
                    (4, 0) => 5,
                    _ => 0,
                },
                0xf03 => 0,
                0x703 => 0,
                0xf05 => (s[5] as u32) * 0x11,
                0x705 => {
                    s[5] = payload & 3;
                    0
                }
                0xf06 => s[6] as u32,
                0x706 => {
                    s[6] = payload;
                    0
                }
                0xf07 => s[7] as u32,
                0x707 => {
                    s[7] = payload;
                    0
                }
                0xf08 => s[8] as u32,
                0x708 => {
                    s[8] = payload;
                    0
                }
                0xf09 => 1 << 31,
                0x709 => 0,
                0xf0c => s[9] as u32,
                0x70c => {
                    s[9] = payload & 7;
                    0
                }
                0xf0d => 0,
                0x70d => 0,
                0xf1c => u32::from_le_bytes(s[12..16].try_into().unwrap()),
                0x71c..=0x71f => {
                    s[12 + (verb - 0x71c) as usize] = payload;
                    0
                }
                0xf20 => 0x1af4_0022,
                0x720..=0x723 => 0,
                0x7ff if node == 1 => {
                    *self = Self::default();
                    0
                }
                _ => 0,
            },
        };
        Some(value)
    }

    fn parameter(node: usize, parameter: u8) -> u32 {
        match (node, parameter) {
            (0, 0) => 0x1af4_0022,
            (0, 2) => 0x0010_0101,
            (0, 4) => 0x0001_0001,
            (1, 4) => 0x0002_0004,
            (1, 5) => 1,
            (1, 8) => 0x0000_0808,
            (2, 9) => 0x0000_041d, // stereo output, amp, override, power
            (3, 9) => 0x0040_0501, // stereo pin, connection, power
            (4, 9) => 0x0010_051b, // stereo input, amp, connection, override, power
            (5, 9) => 0x0040_0401,
            // AC_PAR_PCM: bits 0-10 sample rates (44.1, 48, 96 kHz), bit 17
            // 16-bit samples. Bits 0-4 and 16-20 are widths: 1<<16 is 8-bit,
            // which the kernel maps to SNDRV_PCM_FORMAT_U8 and no 16-bit
            // guest format survives refinement.
            (1 | 2 | 4, 0xa) => (1 << 17) | (1 << 5) | (1 << 6) | (1 << 8),
            (1 | 2 | 4, 0xb) => 1,
            (3, 0xc) => 0x10,
            (5, 0xc) => 0x20,
            (1 | 4, 0xd) | (1 | 2, 0x12) => 0x8003_4a4a,
            (3 | 4, 0xe) => 1,
            (1..=5, 0xf) => 0xf,
            _ => 0,
        }
    }

    /// Whether the converter is live: matching stream tag, matching
    /// converter format, every widget in D0, and the pin enabled in the
    /// stream's direction. EAPD is deliberately absent: it powers a board
    /// amplifier, and the generic parser leaves it untouched — a controller
    /// must not mute the stream for it.
    pub fn connected(&self, input: bool, tag: u8, format: u16) -> bool {
        let converter = if input { 4 } else { 2 } * 24;
        let pin = if input { 5 } else { 3 } * 24;
        tag != 0
            && self.bytes[converter + 6] >> 4 == tag
            && self.bytes[converter + 6] & 0xf == 0
            && u16::from_le_bytes([self.bytes[converter + 10], self.bytes[converter + 11]])
                == format
            && self.bytes[24 + 5] == 0
            && self.bytes[converter + 5] == 0
            && self.bytes[pin + 5] == 0
            && self.bytes[pin + 7] & if input { 0x20 } else { 0x40 } != 0
    }

    pub fn amplify(&self, input: bool, pcm: &mut [u8], channels: usize) {
        let offset = if input { 4 * 24 } else { 2 * 24 + 2 };
        for (i, sample) in pcm.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let amp = self.bytes[offset + i % channels];
            let value = i16::from_le_bytes([sample[0], sample[1]]);
            let scaled = if amp & 0x80 != 0 {
                0
            } else {
                let db = f64::from(i32::from(amp & 0x7f) - 0x4a);
                (f64::from(value) * 10f64.powf(db / 20.0))
                    .round()
                    .clamp(i16::MIN as f64, i16::MAX as f64) as i16
            };
            sample.copy_from_slice(&scaled.to_le_bytes());
        }
    }
}
