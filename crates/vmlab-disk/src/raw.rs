//! Raw images: the file is the disk. No snapshots.

use std::path::Path;

use crate::{DiskImage, Result, SnapshotInfo};

pub struct RawImage {
    _todo: (),
}

impl RawImage {
    pub fn open(_path: &Path, _read_only: bool) -> Result<Self> {
        unimplemented!("raw image open")
    }
}

impl DiskImage for RawImage {
    fn size(&self) -> u64 {
        unimplemented!()
    }
    fn read_only(&self) -> bool {
        unimplemented!()
    }
    fn read_at(&mut self, _offset: u64, _buf: &mut [u8]) -> Result<()> {
        unimplemented!()
    }
    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> Result<()> {
        unimplemented!()
    }
    fn flush(&mut self) -> Result<()> {
        unimplemented!()
    }
    fn discard(&mut self, _offset: u64, _len: u64) -> Result<()> {
        unimplemented!()
    }
    fn snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        unimplemented!()
    }
    fn snapshot_create(
        &mut self,
        _name: &str,
        _vm_state_size: u64,
        _vm_clock_nsec: u64,
    ) -> Result<()> {
        unimplemented!()
    }
    fn snapshot_apply(&mut self, _name: &str) -> Result<SnapshotInfo> {
        unimplemented!()
    }
    fn snapshot_delete(&mut self, _name: &str) -> Result<()> {
        unimplemented!()
    }
    fn write_vmstate(&mut self, _offset: u64, _buf: &[u8]) -> Result<()> {
        unimplemented!()
    }
    fn read_vmstate(&mut self, _offset: u64, _buf: &mut [u8]) -> Result<()> {
        unimplemented!()
    }
}
