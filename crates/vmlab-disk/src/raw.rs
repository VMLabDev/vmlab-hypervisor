//! Raw images: the file is the disk. No snapshots.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{DiskImage, Error, Result, SnapshotInfo};

pub struct RawImage {
    file: File,
    path: PathBuf,
    size: u64,
    read_only: bool,
}

impl RawImage {
    pub fn open(path: &Path, read_only: bool) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let size = file.metadata()?.len();
        Ok(RawImage {
            file,
            path: path.to_path_buf(),
            size,
            read_only,
        })
    }
}

impl DiskImage for RawImage {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| Error::Unsupported("read offset+len overflows u64".into()))?;
        if end > self.size {
            return Err(Error::Unsupported(format!(
                "read beyond end of raw image {} (offset {offset}, len {})",
                self.path.display(),
                buf.len()
            )));
        }
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)?;
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| Error::Unsupported("write offset+len overflows u64".into()))?;
        if end > self.size {
            return Err(Error::Unsupported(format!(
                "write beyond end of raw image {} (offset {offset}, len {})",
                self.path.display(),
                buf.len()
            )));
        }
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(buf)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if !self.read_only {
            self.file.flush()?;
            self.file.sync_all()?;
        }
        Ok(())
    }

    fn discard(&mut self, _offset: u64, _len: u64) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        // A hint; this implementation ignores it.
        Ok(())
    }

    fn snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        Ok(Vec::new())
    }

    fn snapshot_create(
        &mut self,
        _name: &str,
        _vm_state_size: u64,
        _vm_clock_nsec: u64,
    ) -> Result<()> {
        Err(Error::Unsupported("raw images have no snapshots".into()))
    }

    fn snapshot_apply(&mut self, _name: &str) -> Result<SnapshotInfo> {
        Err(Error::Unsupported("raw images have no snapshots".into()))
    }

    fn snapshot_delete(&mut self, _name: &str) -> Result<()> {
        Err(Error::Unsupported("raw images have no snapshots".into()))
    }

    fn write_vmstate(&mut self, _offset: u64, _buf: &[u8]) -> Result<()> {
        Err(Error::Unsupported("raw images have no vmstate area".into()))
    }

    fn read_vmstate(&mut self, _offset: u64, _buf: &mut [u8]) -> Result<()> {
        Err(Error::Unsupported("raw images have no vmstate area".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_image(tag: &str, size: u64) -> (std::path::PathBuf, RawImage) {
        let dir = std::env::temp_dir().join(format!("vmlab-disk-raw-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.raw");
        let f = File::create(&path).unwrap();
        f.set_len(size).unwrap();
        f.sync_all().unwrap();
        drop(f);
        let img = RawImage::open(&path, false).unwrap();
        (dir, img)
    }

    #[test]
    fn round_trip_writes_and_reads() {
        let (_d, mut img) = tmp_image("rw", 8192);
        assert_eq!(img.size(), 8192);
        assert!(!img.read_only());
        let pattern: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        img.write_at(2048, &pattern).unwrap();
        img.flush().unwrap();
        let mut got = vec![0u8; pattern.len()];
        img.read_at(2048, &mut got).unwrap();
        assert_eq!(got, pattern);
        // Unwritten area reads back as zeros (sparse file).
        let mut zeros = vec![0u8; 512];
        img.read_at(0, &mut zeros).unwrap();
        assert!(zeros.iter().all(|&b| b == 0));
    }

    #[test]
    fn out_of_range_is_rejected() {
        let (_d, mut img) = tmp_image("range", 1024);
        assert!(img.read_at(512, &mut vec![0u8; 600]).is_err());
        assert!(img.write_at(1000, &[0u8; 30]).is_err());
        // Exactly at the end is fine.
        img.read_at(512, &mut vec![0u8; 512]).unwrap();
    }

    #[test]
    fn read_only_open_rejects_writes() {
        let (_d, img) = tmp_image("ro", 512);
        let path = img.path.clone();
        drop(img);
        let mut ro = RawImage::open(&path, true).unwrap();
        assert!(ro.read_only());
        assert!(matches!(ro.write_at(0, &[1]), Err(Error::ReadOnly)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn snapshot_ops_are_unsupported() {
        let (_d, mut img) = tmp_image("snap", 512);
        assert!(matches!(
            img.snapshot_create("s", 0, 0),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            img.snapshot_apply("s"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            img.snapshot_delete("s"),
            Err(Error::Unsupported(_))
        ));
        assert!(img.snapshots().unwrap().is_empty());
    }
}
