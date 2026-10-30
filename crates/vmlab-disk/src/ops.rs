//! Image operations vmlab performs with `qemu-img` today (D15): create,
//! linked clone, rebase, info, resize, convert, snapshot, commit. Each is a
//! function over paths so vmlab can link it directly.
//!
//! The behaviours mirror `qemu-img` where vmlab observes them (see
//! `src/template/qimg.rs` in the vmlab tree): a linked clone records an
//! absolute backing path, `rebase -u` records the backing string exactly as
//! given, safe rebase writes only clusters whose old-chain and new-backing
//! content differ, `convert -l … -B …` writes only the clusters allocated in
//! the source's own layer, and `commit -d` leaves the top image untouched.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::qcow2::{Qcow2Image, probe_format};
use crate::{DiskImage, Error, Format, Result};

const DEFAULT_CLUSTER_BITS: u32 = 16;

/// Subset of `qemu-img info --output=json` vmlab reads, plus the full
/// backing chain. Field names match qemu-img so vmlab can consume the JSON
/// unchanged.
#[derive(Debug, Clone, Serialize)]
pub struct ImageInfo {
    pub format: String,
    #[serde(rename = "virtual-size")]
    pub virtual_size: u64,
    #[serde(rename = "actual-size")]
    pub actual_size: u64,
    #[serde(rename = "backing-filename", skip_serializing_if = "Option::is_none")]
    pub backing_file: Option<String>,
    #[serde(
        rename = "full-backing-filename",
        skip_serializing_if = "Option::is_none"
    )]
    pub full_backing_file: Option<PathBuf>,
    #[serde(
        rename = "backing-filename-format",
        skip_serializing_if = "Option::is_none"
    )]
    pub backing_file_format: Option<String>,
    pub snapshots: Vec<SnapshotJson>,
    /// The full backing chain, top image first.
    pub chain: Vec<ChainEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotJson {
    pub id: String,
    pub name: String,
    #[serde(rename = "vm-state-size")]
    pub vm_state_size: u64,
    #[serde(rename = "date-sec")]
    pub date_sec: u64,
    #[serde(rename = "date-nsec")]
    pub date_nsec: u32,
    #[serde(rename = "vm-clock-nsec")]
    pub vm_clock_nsec: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChainEntry {
    pub filename: PathBuf,
    pub format: String,
    #[serde(rename = "virtual-size")]
    pub virtual_size: u64,
}

/// Create a blank qcow2 of `size` bytes, optionally backed by `backing`
/// (recorded exactly as given, like `qemu-img create -b`).
pub fn create_qcow2(
    path: &Path,
    size: u64,
    backing: Option<&Path>,
    backing_format: Option<&str>,
) -> Result<()> {
    let backing_ref = backing.map(|b| (b, backing_format));
    let mut img = Qcow2Image::create(path, size, DEFAULT_CLUSTER_BITS, backing_ref)?;
    img.flush()
}

/// Create a qcow2 linked clone at `dest` backed by `backing`. The backing
/// path is canonicalised so the clone works from any cwd (vmlab relies on
/// this).
pub fn create_linked_clone(backing: &Path, dest: &Path) -> Result<()> {
    let canonical = backing
        .canonicalize()
        .map_err(|e| Error::Unsupported(format!("cannot resolve {}: {e}", backing.display())))?;
    create_qcow2(
        dest,
        probe_size_of(backing)?,
        Some(&canonical),
        Some("qcow2"),
    )
}

fn probe_size_of(path: &Path) -> Result<u64> {
    let fmt = probe_format(path)?;
    let img = crate::open(path, fmt, true)?;
    Ok(img.size())
}

/// Rewrite `path`'s backing-file reference to `backing` (recorded exactly as
/// given, so a relative path stays relative to the image's directory)
/// without touching data: `qemu-img rebase -u`. Only correct when `backing`
/// names the same image the overlay was written against.
pub fn rebase_unsafe(path: &Path, backing: &str, format: Option<&str>) -> Result<()> {
    let mut img = Qcow2Image::open(path, false)?;
    img.set_backing(backing, format)?;
    img.flush()
}

/// Move `path` onto `backing` keeping its content: `qemu-img rebase` in safe
/// mode. Reads the old chain (through the image) and the new backing, and
/// writes into the image only the clusters where they differ.
pub fn rebase_safe(path: &Path, backing: &Path, format: Option<&str>) -> Result<()> {
    let mut img = Qcow2Image::open(path, false)?;
    let fmt = match format {
        Some(f) => format_to_format(f)?,
        None => probe_format(backing)?,
    };
    let mut new_backing = crate::open(backing, fmt, true)?;
    let cluster = img.cluster_size_bytes();
    let size = img.size();
    let mut offset = 0u64;
    while offset < size {
        let n = cluster.min(size - offset);
        let mut old_view = vec![0u8; n as usize];
        img.read_at(offset, &mut old_view)?;
        let mut new_view = vec![0u8; n as usize];
        if offset < new_backing.size() {
            let avail = (new_backing.size() - offset).min(n);
            new_backing.read_at(offset, &mut new_view[..avail as usize])?;
        }
        if old_view != new_view {
            img.write_at(offset, &old_view)?;
        }
        offset += n;
    }
    let backing_str = backing.to_string_lossy().into_owned();
    img.set_backing(&backing_str, format)?;
    img.flush()
}

fn format_to_format(name: &str) -> Result<Format> {
    match name {
        "qcow2" => Ok(Format::Qcow2),
        "raw" => Ok(Format::Raw),
        other => Err(Error::Unsupported(format!(
            "backing format {other} is not supported"
        ))),
    }
}

/// Inspect an image; mirrors the fields vmlab reads from
/// `qemu-img info --output=json` plus the full backing chain.
pub fn info(path: &Path) -> Result<ImageInfo> {
    let fmt = probe_format(path)?;
    let actual_size = actual_size(path)?;
    match fmt {
        Format::Raw => {
            let img = crate::raw::RawImage::open(path, true)?;
            let virtual_size = img.size();
            Ok(ImageInfo {
                format: "raw".into(),
                virtual_size,
                actual_size,
                backing_file: None,
                full_backing_file: None,
                backing_file_format: None,
                snapshots: Vec::new(),
                chain: vec![ChainEntry {
                    filename: path.to_path_buf(),
                    format: "raw".into(),
                    virtual_size,
                }],
            })
        }
        Format::Qcow2 => {
            let img = Qcow2Image::open(path, true)?;
            let virtual_size = img.size();
            let snapshots = img
                .snapshots()?
                .into_iter()
                .map(|s| SnapshotJson {
                    id: s.id,
                    name: s.name,
                    vm_state_size: s.vm_state_size,
                    date_sec: s.date_sec,
                    date_nsec: s.date_nsec,
                    vm_clock_nsec: s.vm_clock_nsec,
                })
                .collect();
            let backing_file = img.backing_file_name().map(str::to_string);
            let full_backing_file = img.backing_file_path();
            let backing_file_format = img.backing_format().map(str::to_string);

            // Walk the chain: top first, each entry as an absolute path.
            let mut chain = vec![ChainEntry {
                filename: absolute(path),
                format: "qcow2".into(),
                virtual_size,
            }];
            let mut cursor = full_backing_file.clone();
            while let Some(bp) = cursor {
                let bfmt = probe_format(&bp)?;
                match bfmt {
                    Format::Raw => {
                        let size = crate::raw::RawImage::open(&bp, true)?.size();
                        chain.push(ChainEntry {
                            filename: bp,
                            format: "raw".into(),
                            virtual_size: size,
                        });
                        cursor = None;
                    }
                    Format::Qcow2 => {
                        let bimg = Qcow2Image::open(&bp, true)?;
                        let next = bimg.backing_file_path();
                        chain.push(ChainEntry {
                            filename: bp,
                            format: "qcow2".into(),
                            virtual_size: bimg.size(),
                        });
                        cursor = next;
                    }
                }
            }

            Ok(ImageInfo {
                format: "qcow2".into(),
                virtual_size,
                actual_size,
                backing_file,
                full_backing_file,
                backing_file_format,
                snapshots,
                chain,
            })
        }
    }
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

#[cfg(unix)]
fn actual_size(path: &Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.blocks() * 512)
}

#[cfg(not(unix))]
fn actual_size(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

/// Grow an image to `new_size` bytes (growing only; shrinking needs
/// `--shrink` in qemu-img and is deliberately not offered, matching vmlab).
pub fn resize(path: &Path, new_size: u64) -> Result<()> {
    match probe_format(path)? {
        Format::Raw => {
            let file = std::fs::OpenOptions::new().write(true).open(path)?;
            let old = file.metadata()?.len();
            if new_size < old {
                return Err(Error::Unsupported(format!(
                    "shrinking a raw image from {old} to {new_size} bytes is not supported"
                )));
            }
            file.set_len(new_size)?;
            Ok(())
        }
        Format::Qcow2 => {
            let mut img = Qcow2Image::open(path, false)?;
            img.resize_grow(new_size)?;
            img.flush()
        }
    }
}

/// Convert `src` (any format) to a fresh qcow2 at `dest`.
///
/// * `snapshot`: read `src` at that internal snapshot (`convert -l`), without
///   modifying the source.
/// * `backing`: create `dest` on top of that backing file (`convert -B`), and
///   copy only the clusters allocated in the source's own layer, so `dest`
///   holds just the delta and reads through its backing for the rest.
pub fn convert(
    src: &Path,
    dest: &Path,
    snapshot: Option<&str>,
    backing: Option<&Path>,
    backing_format: Option<&str>,
) -> Result<()> {
    if dest.exists() {
        std::fs::remove_file(dest)?;
    }
    let backing_ref = backing.map(|b| (b, backing_format));
    match snapshot {
        Some(name) => {
            let mut view = Qcow2Image::open(src, true)?;
            view.install_snapshot_view(name)?;
            let size = view.size();
            let mut dest_img = Qcow2Image::create(dest, size, DEFAULT_CLUSTER_BITS, backing_ref)?;
            if backing.is_some() {
                copy_own_layer(&mut view, &mut dest_img)?;
            } else {
                copy_full(&mut view, &mut dest_img)?;
            }
            dest_img.flush()
        }
        None => match probe_format(src)? {
            Format::Raw => {
                let mut reader = crate::raw::RawImage::open(src, true)?;
                let size = reader.size();
                let mut dest_img =
                    Qcow2Image::create(dest, size, DEFAULT_CLUSTER_BITS, backing_ref)?;
                copy_full(&mut reader, &mut dest_img)?;
                dest_img.flush()
            }
            Format::Qcow2 => {
                let mut reader = Qcow2Image::open(src, true)?;
                let size = reader.size();
                let mut dest_img =
                    Qcow2Image::create(dest, size, DEFAULT_CLUSTER_BITS, backing_ref)?;
                if backing.is_some() {
                    copy_own_layer(&mut reader, &mut dest_img)?;
                } else {
                    copy_full(&mut reader, &mut dest_img)?;
                }
                dest_img.flush()
            }
        },
    }
}

/// Copy the whole virtual disk, skipping all-zero chunks (sparse output,
/// like `qemu-img convert`).
fn copy_full(src: &mut dyn DiskImage, dest: &mut Qcow2Image) -> Result<()> {
    let size = src.size();
    let chunk = 1024 * 1024u64;
    let mut offset = 0u64;
    while offset < size {
        let n = chunk.min(size - offset) as usize;
        let mut buf = vec![0u8; n];
        src.read_at(offset, &mut buf)?;
        if !buf.iter().all(|&b| b == 0) {
            dest.write_at(offset, &buf)?;
        }
        offset += n as u64;
    }
    Ok(())
}

/// Copy only the clusters allocated in `src`'s own layer (`convert -B`).
fn copy_own_layer(src: &mut Qcow2Image, dest: &mut Qcow2Image) -> Result<()> {
    let cluster = src.cluster_size_bytes();
    let size = src.size();
    src.for_each_own_cluster(&mut |src, guest_cluster| {
        let offset = guest_cluster * cluster;
        if offset >= size {
            return Ok(()); // vmstate area never belongs in the copy
        }
        let mut buf = vec![0u8; cluster as usize];
        src.read_at(offset, &mut buf)?;
        dest.write_at(offset, &buf)?;
        Ok(())
    })
}

/// Create a qcow2-internal snapshot on a powered-off image (disk-only, like
/// `qemu-img snapshot -c`).
pub fn snapshot_create(path: &Path, name: &str) -> Result<()> {
    let mut img = Qcow2Image::open(path, false)?;
    img.snapshot_create(name, 0, 0)?;
    img.flush()
}

/// Apply (revert to) a qcow2-internal snapshot.
pub fn snapshot_apply(path: &Path, name: &str) -> Result<crate::SnapshotInfo> {
    let mut img = Qcow2Image::open(path, false)?;
    let info = img.snapshot_apply(name)?;
    img.flush()?;
    Ok(info)
}

/// Delete a qcow2-internal snapshot.
pub fn snapshot_delete(path: &Path, name: &str) -> Result<()> {
    let mut img = Qcow2Image::open(path, false)?;
    img.snapshot_delete(name)?;
    img.flush()
}

/// List a qcow2 image's internal snapshots.
pub fn snapshot_list(path: &Path) -> Result<Vec<crate::SnapshotInfo>> {
    Qcow2Image::open(path, true)?.snapshots()
}

/// Write `top`'s active layer into its backing file, leaving `top` exactly
/// as it was (`qemu-img commit -d`).
pub fn commit(top: &Path) -> Result<()> {
    let img = Qcow2Image::open(top, true)?; // top is only read
    let backing_path = img
        .backing_file_path()
        .ok_or_else(|| Error::Unsupported("image has no backing file to commit into".into()))?;
    let backing_format = match img.backing_format() {
        Some(f) => format_to_format(f)?,
        None => probe_format(&backing_path)?,
    };
    let cluster = img.cluster_size_bytes();
    let top_size = img.size();
    let mut img = img;
    let mut backing = crate::open(&backing_path, backing_format, false)?;
    let common = top_size.min(backing.size());
    img.for_each_own_cluster(&mut |img, guest_cluster| {
        let offset = guest_cluster * cluster;
        if offset >= common {
            return Ok(());
        }
        let mut buf = vec![0u8; cluster as usize];
        img.read_at(offset, &mut buf)?;
        backing.write_at(offset, &buf)?;
        Ok(())
    })?;
    backing.flush()
}
