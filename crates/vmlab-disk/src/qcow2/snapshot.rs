//! qcow2 internal snapshots: the snapshot table, snapshot L1 tables and the
//! vmstate area. See `DESIGN.md` for the QEMU-observed layout this mirrors.

use std::time::{SystemTime, UNIX_EPOCH};

use super::{COMPRESSED_BIT, COPIED_BIT, OFFSET_MASK, Qcow2Image, ZERO_BIT};
use crate::{Error, Result, SnapshotInfo};

/// One snapshot table entry, as stored on disk.
pub(crate) struct SnapshotEntry {
    l1_table_offset: u64,
    l1_size: u32,
    id: String,
    name: String,
    date_sec: u32,
    date_nsec: u32,
    vm_clock_nsec: u64,
    /// Bytes 32-35; ignored when `vm_state_size_large` is present.
    vm_state_size: u32,
    vm_state_size_large: Option<u64>,
    /// Virtual disk size at snapshot time (extra data, if present).
    disk_size: Option<u64>,
}

impl SnapshotEntry {
    fn effective_vm_state_size(&self) -> u64 {
        self.vm_state_size_large
            .unwrap_or(self.vm_state_size as u64)
    }

    fn info(&self) -> SnapshotInfo {
        SnapshotInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            vm_state_size: self.effective_vm_state_size(),
            date_sec: self.date_sec as u64,
            date_nsec: self.date_nsec,
            vm_clock_nsec: self.vm_clock_nsec,
        }
    }

    /// Serialized size, padded to a multiple of 8 as the format requires.
    fn padded_size(&self) -> usize {
        (40 + 24 + self.id.len() + self.name.len()).div_ceil(8) * 8
    }

    fn serialize(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&self.l1_table_offset.to_be_bytes());
        out.extend_from_slice(&self.l1_size.to_be_bytes());
        out.extend_from_slice(&(self.id.len() as u16).to_be_bytes());
        out.extend_from_slice(&(self.name.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.date_sec.to_be_bytes());
        out.extend_from_slice(&self.date_nsec.to_be_bytes());
        out.extend_from_slice(&self.vm_clock_nsec.to_be_bytes());
        out.extend_from_slice(&self.vm_state_size.to_be_bytes());
        out.extend_from_slice(&24u32.to_be_bytes()); // extra data size
        out.extend_from_slice(&self.effective_vm_state_size().to_be_bytes());
        out.extend_from_slice(&self.disk_size.unwrap_or(0).to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes()); // icount: disabled
        out.extend_from_slice(self.id.as_bytes());
        out.extend_from_slice(self.name.as_bytes());
        while !(out.len() - start).is_multiple_of(8) {
            out.push(0);
        }
    }
}

impl Qcow2Image {
    /// Parse the snapshot table with a private read handle (`snapshots()` is
    /// `&self`).
    fn parse_table(&self) -> Result<Vec<SnapshotEntry>> {
        if self.nb_snapshots == 0 {
            return Ok(Vec::new());
        }
        let mut file = std::fs::File::open(&self.path)?;
        use std::io::{Read, Seek, SeekFrom};
        let mut off = self.snapshots_offset;
        let mut entries = Vec::with_capacity(self.nb_snapshots as usize);
        for _ in 0..self.nb_snapshots {
            let mut fixed = [0u8; 40];
            file.seek(SeekFrom::Start(off))?;
            file.read_exact(&mut fixed)?;
            let l1_table_offset = u64::from_be_bytes(fixed[0..8].try_into().unwrap());
            let l1_size = u32::from_be_bytes(fixed[8..12].try_into().unwrap());
            let id_len = u16::from_be_bytes(fixed[12..14].try_into().unwrap()) as usize;
            let name_len = u16::from_be_bytes(fixed[14..16].try_into().unwrap()) as usize;
            let date_sec = u32::from_be_bytes(fixed[16..20].try_into().unwrap());
            let date_nsec = u32::from_be_bytes(fixed[20..24].try_into().unwrap());
            let vm_clock_nsec = u64::from_be_bytes(fixed[24..32].try_into().unwrap());
            let vm_state_size = u32::from_be_bytes(fixed[32..36].try_into().unwrap());
            let extra = u32::from_be_bytes(fixed[36..40].try_into().unwrap()) as usize;
            let mut extra_buf = vec![0u8; extra];
            file.read_exact(&mut extra_buf)?;
            let vm_state_size_large =
                (extra >= 8).then(|| u64::from_be_bytes(extra_buf[0..8].try_into().unwrap()));
            let disk_size =
                (extra >= 16).then(|| u64::from_be_bytes(extra_buf[8..16].try_into().unwrap()));
            let mut ids = vec![0u8; id_len + name_len];
            file.read_exact(&mut ids)?;
            let id = String::from_utf8_lossy(&ids[..id_len]).into_owned();
            let name = String::from_utf8_lossy(&ids[id_len..]).into_owned();
            entries.push(SnapshotEntry {
                l1_table_offset,
                l1_size,
                id,
                name,
                date_sec,
                date_nsec,
                vm_clock_nsec,
                vm_state_size,
                vm_state_size_large,
                disk_size,
            });
            off += ((40 + extra + id_len + name_len) as u64).div_ceil(8) * 8;
        }
        Ok(entries)
    }

    pub(crate) fn list_snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        Ok(self.parse_table()?.into_iter().map(|e| e.info()).collect())
    }

    /// Read a snapshot's L1 table (raw values), its info and recorded disk
    /// size.
    pub(crate) fn load_snapshot_l1(
        &mut self,
        name: &str,
    ) -> Result<(SnapshotInfo, Vec<u64>, Option<u64>)> {
        let entries = self.parse_table()?;
        let entry = entries
            .iter()
            .find(|e| e.name == name || e.id == name)
            .ok_or_else(|| Error::SnapshotNotFound(name.to_string()))?;
        let info = entry.info();
        let disk_size = entry.disk_size;
        let l1_size = entry.l1_size;
        let l1_offset = entry.l1_table_offset;
        let mut bytes = vec![0u8; l1_size as usize * 8];
        self.read_file(l1_offset, &mut bytes)?;
        let l1 = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_be_bytes(*c))
            .collect();
        Ok((info, l1, disk_size))
    }

    // ----- snapshot create -----

    pub(crate) fn snapshot_create_impl(
        &mut self,
        name: &str,
        vm_state_size: u64,
        vm_clock_nsec: u64,
    ) -> Result<()> {
        self.check_writable()?;
        if name.is_empty() || name.len() > 1023 {
            return Err(Error::Unsupported(
                "snapshot name must be 1..=1023 bytes".into(),
            ));
        }
        let entries = self.parse_table()?;
        if entries.iter().any(|e| e.name == name) {
            return Err(Error::Unsupported(format!(
                "snapshot {name:?} already exists"
            )));
        }
        let id = next_snapshot_id(&entries);

        // The active L1 must cover the vmstate area too.
        if vm_state_size > 0 {
            let end = self.vmstate_base() + vm_state_size;
            self.ensure_l1_covers(end)?;
        }

        // Everything reachable from the active L1 becomes shared with the
        // snapshot that will point at the same L2 tables and clusters.
        let snap_l1_pre = self.l1.clone();
        self.walk_refcounts(&snap_l1_pre, 1)?;

        // Punch the vmstate out of the active layer; the snapshot L1 (cloned
        // above) keeps pointing at the original L2 tables and data.
        if vm_state_size > 0 {
            self.punch_vmstate(vm_state_size)?;
        }

        self.rewrite_active_copied_bits()?;

        // Snapshot L1 values with copied bits recomputed from the final
        // refcounts (vmstate L2s are back to rc 1; the rest is shared).
        let mut snap_l1 = snap_l1_pre;
        for entry in &mut snap_l1 {
            let l2_host = *entry & OFFSET_MASK;
            if l2_host == 0 {
                continue;
            }
            let want = if self.get_refcount(l2_host / self.cluster_size)? == 1 {
                COPIED_BIT
            } else {
                0
            };
            *entry = (*entry & !COPIED_BIT) | want;
        }

        let clusters = self.l1_bytes_clusters(snap_l1.len() as u32);
        let snap_offset = self.alloc_clusters_contiguous(clusters)?;
        let mut bytes = Vec::with_capacity(clusters as usize * self.cluster_size as usize);
        for e in &snap_l1 {
            bytes.extend_from_slice(&e.to_be_bytes());
        }
        bytes.resize(clusters as usize * self.cluster_size as usize, 0);
        self.write_file(snap_offset, &bytes)?;
        for idx in 0..clusters {
            self.set_cluster_refcount(snap_offset / self.cluster_size + idx, 1)?;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mut new_entries = entries;
        new_entries.push(SnapshotEntry {
            l1_table_offset: snap_offset,
            l1_size: snap_l1.len() as u32,
            id,
            name: name.to_string(),
            date_sec: now.as_secs() as u32,
            date_nsec: now.subsec_nanos(),
            vm_clock_nsec,
            vm_state_size: vm_state_size.min(u32::MAX as u64) as u32,
            vm_state_size_large: Some(vm_state_size),
            disk_size: Some(self.size),
        });
        self.rewrite_snapshot_table(&new_entries)?;
        self.flush_metadata()?;
        self.file.sync_all()?;
        Ok(())
    }

    fn l1_bytes_clusters(&self, entries: u32) -> u64 {
        ((entries as u64) * 8).div_ceil(self.cluster_size).max(1)
    }

    /// Replace the vmstate entries of the active L1 with zero-flag
    /// descriptors, COWing L2 tables that became shared.
    fn punch_vmstate(&mut self, vm_state_size: u64) -> Result<()> {
        let base = self.vmstate_base();
        let end = base + vm_state_size;
        let shift = self.cluster_bits + (self.cluster_bits - 3);
        let first_l1 = (base >> shift) as usize;
        let last_l1 = ((end - 1) >> shift) as usize;
        for l1_index in first_l1..=last_l1.min(self.l1.len().saturating_sub(1)) {
            let l2_host = self.l1[l1_index] & OFFSET_MASK;
            if l2_host == 0 {
                continue;
            }
            // COW the L2 if it is now shared with the snapshot L1.
            let active_l2 = if self.get_refcount(l2_host / self.cluster_size)? > 1 {
                let content = self.read_cluster_via_cache(l2_host)?;
                let new_l2 = self.alloc_cluster()?;
                self.write_file(new_l2, &content)?;
                self.set_cluster_refcount(new_l2 / self.cluster_size, 1)?;
                self.dec_refcount(l2_host / self.cluster_size)?;
                self.l1[l1_index] = new_l2 | COPIED_BIT;
                self.l1_dirty = true;
                new_l2
            } else {
                l2_host
            };
            // Zero-flag every vmstate cluster this entry maps.
            let l2e = self.l2_entries();
            let entry_start = l1_index as u64 * l2e * self.cluster_size;
            let idx_start = if base > entry_start {
                (base - entry_start) / self.cluster_size
            } else {
                0
            };
            let idx_end = ((end - entry_start) / self.cluster_size).min(l2e);
            for l2_index in idx_start..idx_end {
                let entry = self.l2_entry(active_l2, l2_index)?;
                if entry == 0 {
                    continue;
                }
                if entry & COMPRESSED_BIT != 0 {
                    self.adjust_compressed_refcounts(entry, -1)?;
                } else {
                    let host = entry & OFFSET_MASK;
                    if host != 0 {
                        self.dec_refcount(host / self.cluster_size)?;
                    }
                }
                self.set_l2_entry(active_l2, l2_index, ZERO_BIT)?;
            }
        }
        Ok(())
    }

    /// Recompute bit 63 of the active L1 entries and of the entries in every
    /// L2 table reachable from them, from the refcount table. The copied bit
    /// is only accurate in the active view, so it must be rebuilt whenever
    /// the active L1 or the refcounts change.
    pub(crate) fn rewrite_active_copied_bits(&mut self) -> Result<()> {
        for l1_index in 0..self.l1.len() {
            let l2_host = self.l1[l1_index] & OFFSET_MASK;
            if l2_host == 0 {
                continue;
            }
            let want = if self.get_refcount(l2_host / self.cluster_size)? == 1 {
                COPIED_BIT
            } else {
                0
            };
            if self.l1[l1_index] & COPIED_BIT != want {
                self.l1[l1_index] = (self.l1[l1_index] & !COPIED_BIT) | want;
                self.l1_dirty = true;
            }
            self.load_l2(l2_host)?;
            let entries: Vec<u64> = {
                let (buf, _) = self.l2_cache.get(l2_host).expect("cached");
                buf.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| u64::from_be_bytes(*c))
                    .collect()
            };
            for (l2_index, entry) in entries.into_iter().enumerate() {
                if entry & COMPRESSED_BIT != 0 {
                    continue;
                }
                let host = entry & OFFSET_MASK;
                if host == 0 {
                    continue;
                }
                let want = if self.get_refcount(host / self.cluster_size)? == 1 {
                    COPIED_BIT
                } else {
                    0
                };
                if entry & COPIED_BIT != want {
                    self.set_l2_entry(l2_host, l2_index as u64, (entry & !COPIED_BIT) | want)?;
                }
            }
        }
        Ok(())
    }

    // ----- snapshot apply -----

    pub(crate) fn snapshot_apply_impl(&mut self, name: &str) -> Result<SnapshotInfo> {
        self.check_writable()?;
        let entries = self.parse_table()?;
        let entry = entries
            .iter()
            .find(|e| e.name == name || e.id == name)
            .ok_or_else(|| Error::SnapshotNotFound(name.to_string()))?;
        let info = entry.info();
        let snap_l1_size = entry.l1_size;
        let snap_l1_offset = entry.l1_table_offset;
        let snap_disk_size = entry.disk_size;
        let mut bytes = vec![0u8; snap_l1_size as usize * 8];
        self.read_file(snap_l1_offset, &mut bytes)?;
        let snap_l1: Vec<u64> = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_be_bytes(*c))
            .collect();

        // Raise the snapshot's references first so clusters referenced by
        // both the old and the new view never drop to zero.
        self.walk_refcounts(&snap_l1, 1)?;
        let old_active = self.l1.clone();
        self.walk_refcounts(&old_active, -1)?;

        // Install the snapshot's L1 as the active table.
        self.ensure_l1_size(snap_l1_size)?;
        self.l1 = snap_l1;
        self.l1_size = snap_l1_size;
        if let Some(disk_size) = snap_disk_size
            && disk_size != self.size
        {
            self.size = disk_size;
        }
        self.header_dirty = true;
        self.l1_dirty = true;

        self.rewrite_active_copied_bits()?;
        self.flush_metadata()?;
        self.file.sync_all()?;
        Ok(info)
    }

    // ----- snapshot delete -----

    pub(crate) fn snapshot_delete_impl(&mut self, name: &str) -> Result<()> {
        self.check_writable()?;
        let entries = self.parse_table()?;
        let pos = entries
            .iter()
            .position(|e| e.name == name || e.id == name)
            .ok_or_else(|| Error::SnapshotNotFound(name.to_string()))?;
        let entry_l1_offset = entries[pos].l1_table_offset;
        let entry_l1_size = entries[pos].l1_size;

        let mut bytes = vec![0u8; entry_l1_size as usize * 8];
        self.read_file(entry_l1_offset, &mut bytes)?;
        let snap_l1: Vec<u64> = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_be_bytes(*c))
            .collect();
        self.walk_refcounts(&snap_l1, -1)?;
        for idx in 0..self.l1_bytes_clusters(entry_l1_size) {
            self.dec_refcount(entry_l1_offset / self.cluster_size + idx)?;
        }

        let mut remaining = entries;
        remaining.remove(pos);
        self.rewrite_snapshot_table(&remaining)?;
        // Clusters that were shared with this snapshot may now be
        // exclusively owned by the active layer again.
        self.rewrite_active_copied_bits()?;
        self.flush_metadata()?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Write the full snapshot table to a fresh region and free the old one.
    fn rewrite_snapshot_table(&mut self, entries: &[SnapshotEntry]) -> Result<()> {
        let old_offset = self.snapshots_offset;
        // Cluster count of the table currently on disk (header still points
        // at it).
        let old_clusters = if old_offset != 0 {
            Some(self.old_snapshot_table_clusters()?)
        } else {
            None
        };
        let mut bytes = Vec::new();
        for e in entries {
            e.serialize(&mut bytes);
        }
        if bytes.is_empty() {
            self.nb_snapshots = 0;
            self.snapshots_offset = 0;
            self.header_dirty = true;
            if let Some(clusters) = old_clusters {
                for idx in 0..clusters {
                    self.dec_refcount(old_offset / self.cluster_size + idx)?;
                }
            }
            return Ok(());
        }
        let clusters = bytes.len().div_ceil(self.cluster_size as usize) as u64;
        let new_offset = self.alloc_clusters_contiguous(clusters)?;
        bytes.resize(clusters as usize * self.cluster_size as usize, 0);
        self.write_file(new_offset, &bytes)?;
        for idx in 0..clusters {
            self.set_cluster_refcount(new_offset / self.cluster_size + idx, 1)?;
        }
        if let Some(clusters) = old_clusters {
            for idx in 0..clusters {
                self.dec_refcount(old_offset / self.cluster_size + idx)?;
            }
        }
        self.nb_snapshots = entries.len() as u32;
        self.snapshots_offset = new_offset;
        self.header_dirty = true;
        Ok(())
    }

    /// Clusters occupied by the snapshot table currently on disk.
    fn old_snapshot_table_clusters(&mut self) -> Result<u64> {
        let entries = self.parse_table()?;
        let len: usize = entries.iter().map(|e| e.padded_size()).sum();
        Ok((len as u64).div_ceil(self.cluster_size).max(1))
    }
}

fn next_snapshot_id(entries: &[SnapshotEntry]) -> String {
    let max = entries
        .iter()
        .filter_map(|e| e.id.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    (max + 1).to_string()
}
