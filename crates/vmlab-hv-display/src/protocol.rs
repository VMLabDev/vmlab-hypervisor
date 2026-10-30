//! Explicit little-endian codec. Rust memory layout is never sent on the wire.

use crate::invalid;
use std::io::{self, Read};

pub const VERSION: u32 = 1;
pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;
pub const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
pub const ARGB8888: u32 = u32::from_le_bytes(*b"AR24");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities(pub u32);
impl Capabilities {
    pub const FRAMES: u32 = 1;
    pub const CURSOR: u32 = 2;
    pub const INPUT: u32 = 4;
    pub const DMABUF_OK: u32 = 8;
    pub const CPU_COPY: u32 = 16;
    pub const ALL: Self = Self(31);
    pub fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceKind {
    Shm = 0,
    Dmabuf = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Each variant's exact payload is specified in `PROTOCOL.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Hello {
        version: u32,
        capabilities: Capabilities,
    },
    SurfaceConfigure {
        surface_id: u64,
        width: u32,
        height: u32,
        drm_fourcc: u32,
        stride: u32,
        modifier: u64,
        kind: SurfaceKind,
        buffers: u32,
    },
    FrameReady {
        surface_id: u64,
        buffer: u32,
        seq: u64,
        timestamp_ns: u64,
        damage: Vec<Rect>,
    },
    Release {
        surface_id: u64,
        buffer: u32,
        seq: u64,
    },
    CursorShape {
        width: u32,
        height: u32,
        hot_x: u32,
        hot_y: u32,
        argb: Vec<u8>,
    },
    CursorState {
        x: i32,
        y: i32,
        visible: bool,
    },
    CpuCopy {
        surface_id: u64,
    },
    Key {
        qcode: String,
        down: bool,
    },
    PointerAbs {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    Buttons {
        mask: u32,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    LedState {
        mask: u32,
    },
    RequestSize {
        width: u32,
        height: u32,
    },
}

fn u32put(out: &mut Vec<u8>, v: u32) {
    out.extend(v.to_le_bytes());
}
fn u64put(out: &mut Vec<u8>, v: u64) {
    out.extend(v.to_le_bytes());
}
fn u32get(input: &mut &[u8]) -> io::Result<u32> {
    let mut b = [0; 4];
    input.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn u64get(input: &mut &[u8]) -> io::Result<u64> {
    let mut b = [0; 8];
    input.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn boolean(input: &mut &[u8]) -> io::Result<bool> {
    match u32get(input)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid("invalid boolean")),
    }
}

impl Message {
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let mut p = Vec::new();
        let kind: u16 = match self {
            Self::Hello {
                version,
                capabilities,
            } => {
                u32put(&mut p, *version);
                u32put(&mut p, capabilities.0);
                1
            }
            Self::SurfaceConfigure {
                surface_id,
                width,
                height,
                drm_fourcc,
                stride,
                modifier,
                kind,
                buffers,
            } => {
                u64put(&mut p, *surface_id);
                for v in [width, height, drm_fourcc, stride] {
                    u32put(&mut p, *v);
                }
                u64put(&mut p, *modifier);
                u32put(&mut p, *kind as u32);
                u32put(&mut p, *buffers);
                2
            }
            Self::FrameReady {
                surface_id,
                buffer,
                seq,
                timestamp_ns,
                damage,
            } => {
                u64put(&mut p, *surface_id);
                u32put(&mut p, *buffer);
                u64put(&mut p, *seq);
                u64put(&mut p, *timestamp_ns);
                u32put(
                    &mut p,
                    u32::try_from(damage.len()).map_err(|_| invalid("too many rectangles"))?,
                );
                for r in damage {
                    for v in [r.x, r.y, r.width, r.height] {
                        u32put(&mut p, v);
                    }
                }
                3
            }
            Self::Release {
                surface_id,
                buffer,
                seq,
            } => {
                u64put(&mut p, *surface_id);
                u32put(&mut p, *buffer);
                u64put(&mut p, *seq);
                4
            }
            Self::CursorShape {
                width,
                height,
                hot_x,
                hot_y,
                argb,
            } => {
                for v in [width, height, hot_x, hot_y] {
                    u32put(&mut p, *v);
                }
                p.extend(argb);
                5
            }
            Self::CursorState { x, y, visible } => {
                u32put(&mut p, *x as u32);
                u32put(&mut p, *y as u32);
                u32put(&mut p, u32::from(*visible));
                6
            }
            Self::CpuCopy { surface_id } => {
                u64put(&mut p, *surface_id);
                7
            }
            Self::Key { qcode, down } => {
                u32put(&mut p, u32::from(*down));
                u32put(&mut p, qcode.len() as u32);
                p.extend(qcode.as_bytes());
                8
            }
            Self::PointerAbs {
                x,
                y,
                width,
                height,
            } => {
                for v in [x, y, width, height] {
                    u32put(&mut p, *v);
                }
                9
            }
            Self::Buttons { mask } => {
                u32put(&mut p, *mask);
                10
            }
            Self::Scroll { dx, dy } => {
                u32put(&mut p, *dx as u32);
                u32put(&mut p, *dy as u32);
                11
            }
            Self::LedState { mask } => {
                u32put(&mut p, *mask);
                12
            }
            Self::RequestSize { width, height } => {
                u32put(&mut p, *width);
                u32put(&mut p, *height);
                13
            }
        };
        if p.len() + 8 > MAX_MESSAGE {
            return Err(invalid("message too large"));
        }
        let mut out = Vec::with_capacity(p.len() + 8);
        u32put(&mut out, (p.len() + 8) as u32);
        out.extend(kind.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(p);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 8 || bytes.len() > MAX_MESSAGE {
            return Err(invalid("invalid message size"));
        }
        let mut p = bytes;
        if u32get(&mut p)? as usize != bytes.len() {
            return Err(invalid("length mismatch"));
        }
        let kind = u32get(&mut p)?;
        let msg = match kind {
            1 => Self::Hello {
                version: u32get(&mut p)?,
                capabilities: Capabilities(u32get(&mut p)?),
            },
            2 => Self::SurfaceConfigure {
                surface_id: u64get(&mut p)?,
                width: u32get(&mut p)?,
                height: u32get(&mut p)?,
                drm_fourcc: u32get(&mut p)?,
                stride: u32get(&mut p)?,
                modifier: u64get(&mut p)?,
                kind: match u32get(&mut p)? {
                    0 => SurfaceKind::Shm,
                    1 => SurfaceKind::Dmabuf,
                    _ => return Err(invalid("invalid surface kind")),
                },
                buffers: u32get(&mut p)?,
            },
            3 => {
                let surface_id = u64get(&mut p)?;
                let buffer = u32get(&mut p)?;
                let seq = u64get(&mut p)?;
                let timestamp_ns = u64get(&mut p)?;
                let n = u32get(&mut p)? as usize;
                if n > p.len() / 16 {
                    return Err(invalid("invalid damage count"));
                }
                let mut damage = Vec::with_capacity(n);
                for _ in 0..n {
                    damage.push(Rect {
                        x: u32get(&mut p)?,
                        y: u32get(&mut p)?,
                        width: u32get(&mut p)?,
                        height: u32get(&mut p)?,
                    });
                }
                Self::FrameReady {
                    surface_id,
                    buffer,
                    seq,
                    timestamp_ns,
                    damage,
                }
            }
            4 => Self::Release {
                surface_id: u64get(&mut p)?,
                buffer: u32get(&mut p)?,
                seq: u64get(&mut p)?,
            },
            5 => {
                let width = u32get(&mut p)?;
                let height = u32get(&mut p)?;
                let hot_x = u32get(&mut p)?;
                let hot_y = u32get(&mut p)?;
                let n = u64::from(width)
                    .checked_mul(u64::from(height))
                    .and_then(|n| n.checked_mul(4))
                    .ok_or_else(|| invalid("cursor size overflow"))?;
                if n != p.len() as u64
                    || width > 1024
                    || height > 1024
                    || (width == 0) != (height == 0)
                    || (width != 0 && hot_x >= width)
                    || (height != 0 && hot_y >= height)
                {
                    return Err(invalid("invalid cursor"));
                }
                let argb = p.to_vec();
                p = &[];
                Self::CursorShape {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    argb,
                }
            }
            6 => Self::CursorState {
                x: u32get(&mut p)? as i32,
                y: u32get(&mut p)? as i32,
                visible: boolean(&mut p)?,
            },
            7 => Self::CpuCopy {
                surface_id: u64get(&mut p)?,
            },
            8 => {
                let down = boolean(&mut p)?;
                let n = u32get(&mut p)? as usize;
                if n == 0
                    || n > 64
                    || p.len() != n
                    || !p.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
                {
                    return Err(invalid("invalid qcode"));
                }
                let qcode = String::from_utf8(p.to_vec()).map_err(|_| invalid("invalid qcode"))?;
                p = &[];
                Self::Key { qcode, down }
            }
            9 => Self::PointerAbs {
                x: u32get(&mut p)?,
                y: u32get(&mut p)?,
                width: u32get(&mut p)?,
                height: u32get(&mut p)?,
            },
            10 => Self::Buttons {
                mask: u32get(&mut p)?,
            },
            11 => Self::Scroll {
                dx: u32get(&mut p)? as i32,
                dy: u32get(&mut p)? as i32,
            },
            12 => Self::LedState {
                mask: u32get(&mut p)?,
            },
            13 => Self::RequestSize {
                width: u32get(&mut p)?,
                height: u32get(&mut p)?,
            },
            _ => return Err(invalid("unknown message kind or flags")),
        };
        if !p.is_empty() {
            return Err(invalid("trailing message bytes"));
        }
        Ok(msg)
    }
}
