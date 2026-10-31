//! The disk-format seam (D8) and its implementations.
//!
//! Everything above this crate talks to [`DiskImage`]; qcow2 is one
//! implementation and must not leak past it, so a purpose-built format can
//! replace it later. Snapshots are an operation of the format (D14): disk state
//! is a format-level snapshot, and RAM plus device state ride the snapshot's
//! vmstate area, opaque to this crate.
//!
//! The vmstate protocol mirrors qcow2's: before creating a snapshot the caller
//! streams the state into the image's vmstate area with
//! [`DiskImage::write_vmstate`], then calls [`DiskImage::snapshot_create`] with
//! the number of bytes written. After [`DiskImage::snapshot_apply`] the caller
//! reads it back with [`DiskImage::read_vmstate`].

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

pub mod ops;
pub mod qcow2;
pub mod raw;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a {format} image: {reason}")]
    Format {
        format: &'static str,
        reason: String,
    },
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("snapshot not found: {0}")]
    SnapshotNotFound(String),
    #[error("image is read-only")]
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Raw,
    Qcow2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub id: String,
    pub name: String,
    /// Bytes of vmstate recorded with the snapshot; 0 for a disk-only snapshot.
    pub vm_state_size: u64,
    /// Seconds since the Unix epoch.
    pub date_sec: u64,
    pub date_nsec: u32,
    /// Guest run time at capture, nanoseconds.
    pub vm_clock_nsec: u64,
}

/// Destination of a disk read. Implementations append exactly the requested
/// bytes or return an error. Guest-memory targets can read files directly
/// without creating Rust slices over memory another vCPU can modify.
pub trait ReadTarget {
    fn write_all(&mut self, bytes: &[u8]) -> Result<()>;

    fn zero(&mut self, mut len: usize) -> Result<()> {
        let zeros = [0; 65536];
        while len != 0 {
            let count = len.min(zeros.len());
            self.write_all(&zeros[..count])?;
            len -= count;
        }
        Ok(())
    }

    fn read_file(&mut self, file: &File, mut offset: u64, mut len: usize) -> Result<()> {
        let mut scratch = [0; 65536];
        while len != 0 {
            let count = len.min(scratch.len());
            file.read_exact_at(&mut scratch[..count], offset)?;
            self.write_all(&scratch[..count])?;
            offset += count as u64;
            len -= count;
        }
        Ok(())
    }
}

pub(crate) fn check_read(offset: u64, len: usize, size: u64) -> Result<()> {
    if offset
        .checked_add(len as u64)
        .is_some_and(|end| end <= size)
    {
        Ok(())
    } else {
        Err(Error::Unsupported("disk read is out of range".into()))
    }
}

/// One open disk image. Offsets are guest-visible byte offsets.
pub trait DiskImage: Send {
    /// Guest-visible size in bytes.
    fn size(&self) -> u64;
    fn read_only(&self) -> bool;
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;
    /// Read `len` bytes into a target, preserving backing and zero semantics.
    /// An error can leave part of the target written, as with `read_at`.
    fn read_into(
        &mut self,
        mut offset: u64,
        mut len: usize,
        target: &mut dyn ReadTarget,
    ) -> Result<()> {
        check_read(offset, len, self.size())?;
        let mut scratch = [0; 65536];
        while len != 0 {
            let count = len.min(scratch.len());
            self.read_at(offset, &mut scratch[..count])?;
            target.write_all(&scratch[..count])?;
            offset += count as u64;
            len -= count;
        }
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
    /// Hint that a range is unused. Implementations may ignore it.
    fn discard(&mut self, offset: u64, len: u64) -> Result<()>;

    fn snapshots(&self) -> Result<Vec<SnapshotInfo>>;
    /// Record the current disk state as `name`. `vm_state_size` is the number
    /// of vmstate bytes written since the last snapshot operation, 0 for none.
    fn snapshot_create(&mut self, name: &str, vm_state_size: u64, vm_clock_nsec: u64)
    -> Result<()>;
    /// Revert the disk to `name`. Its vmstate becomes readable.
    fn snapshot_apply(&mut self, name: &str) -> Result<SnapshotInfo>;
    fn snapshot_delete(&mut self, name: &str) -> Result<()>;
    fn write_vmstate(&mut self, offset: u64, buf: &[u8]) -> Result<()>;
    fn read_vmstate(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;
}

/// Open an image, following its backing chain.
pub fn open(path: &Path, format: Format, read_only: bool) -> Result<Box<dyn DiskImage>> {
    match format {
        Format::Raw => Ok(Box::new(raw::RawImage::open(path, read_only)?)),
        Format::Qcow2 => Ok(Box::new(qcow2::Qcow2Image::open(path, read_only)?)),
    }
}
