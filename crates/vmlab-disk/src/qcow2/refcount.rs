//! qcow2 host-cluster refcounts and the free-cluster allocator.
//!
//! Invariants kept for `qemu-img check` to pass:
//! - every cluster referenced by any structure (header cluster, refcount
//!   table, refblocks, active and snapshot L1 tables, L2 tables, data
//!   clusters, compressed-cluster byte ranges, snapshot table) carries a
//!   matching refcount;
//! - metadata regions (L1, refcount table, snapshot L1, snapshot table) are
//!   contiguous runs of whole clusters.

use std::collections::BTreeSet;

use super::{OFFSET_MASK, Qcow2Image};
use crate::{Error, Result};

/// Free host clusters the allocator may hand out, plus the cursor of the
/// lazy scan that feeds the set.
///
/// qcow2 has no on-disk free list: a cluster is free exactly when its
/// refcount is zero. The set is therefore derived from the refblocks and
/// rebuilt lazily — at open it is empty and `scan_next` is 0, refblocks are
/// read only as the set drains (bounded per pass), so opening a large image
/// costs nothing until it first allocates. Every refcount write keeps the
/// set exact (zero inserts, non-zero removes), which is what makes holes
/// freed by an earlier session reusable: the scan classifies them from the
/// refblocks the same way this session's own frees do.
#[derive(Default)]
pub(crate) struct FreeClusters {
    set: BTreeSet<u64>,
    /// First cluster index the scan has not classified yet.
    scan_next: u64,
}

/// Free clusters a bounded scan pass collects before it stops early;
/// later passes continue from `scan_next` as the set drains.
const FREE_SCAN_TARGET: usize = 4096;

/// Read one refcount of `width` bits out of a refcount block.
fn bits_get(buf: &[u8], index: u64, width: u32) -> u64 {
    match width {
        1 | 2 | 4 | 8 => {
            let bit = index * width as u64;
            let byte = buf[(bit / 8) as usize];
            let mask = (1u16 << width) - 1;
            ((byte >> (bit % 8)) & mask as u8) as u64
        }
        16 => {
            let o = index as usize * 2;
            u16::from_be_bytes(buf[o..o + 2].try_into().unwrap()) as u64
        }
        32 => {
            let o = index as usize * 4;
            u32::from_be_bytes(buf[o..o + 4].try_into().unwrap()) as u64
        }
        64 => {
            let o = index as usize * 8;
            u64::from_be_bytes(buf[o..o + 8].try_into().unwrap())
        }
        _ => unreachable!("refcount_order <= 6 checked at open"),
    }
}

/// Write one refcount of `width` bits into a refcount block.
fn bits_set(buf: &mut [u8], index: u64, width: u32, value: u64) {
    match width {
        1 | 2 | 4 | 8 => {
            let bit = index * width as u64;
            let byte = &mut buf[(bit / 8) as usize];
            let mask = ((1u16 << width) - 1) as u8;
            *byte &= !(mask << (bit % 8));
            *byte |= ((value as u8) & mask) << (bit % 8);
        }
        16 => {
            let o = index as usize * 2;
            buf[o..o + 2].copy_from_slice(&(value as u16).to_be_bytes());
        }
        32 => {
            let o = index as usize * 4;
            buf[o..o + 4].copy_from_slice(&(value as u32).to_be_bytes());
        }
        64 => {
            let o = index as usize * 8;
            buf[o..o + 8].copy_from_slice(&value.to_be_bytes());
        }
        _ => unreachable!("refcount_order <= 6 checked at open"),
    }
}

impl Qcow2Image {
    /// Clusters covered by one refcount block.
    pub(crate) fn refblock_entries(&self) -> u64 {
        self.cluster_size * 8 / self.refcount_bits as u64
    }

    fn refcount_max(&self) -> u64 {
        (1u64 << self.refcount_bits) - 1
    }

    fn load_refblock(&mut self, table_index: u64) -> Result<()> {
        if self.refblock_cache.get(table_index).is_some() {
            return Ok(());
        }
        let host = self.refblock_host(table_index)?;
        let mut buf = vec![0u8; self.cluster_size as usize];
        self.read_file(host, &mut buf)?;
        // Dirty evictees must reach the disk before their clusters can be
        // handed out again.
        for (index, block) in self.refblock_cache.put(table_index, buf, false) {
            let host = self.refblock_host(index)?;
            self.write_file(host, &block)?;
        }
        Ok(())
    }

    /// Refcount of a cluster; 0 when its refblock is not allocated.
    fn refblock_get(&mut self, table_index: u64, idx_in_block: u64) -> Result<u64> {
        let entry = self
            .refcount_table
            .get(table_index as usize)
            .copied()
            .unwrap_or(0);
        if entry & OFFSET_MASK == 0 {
            return Ok(0);
        }
        self.load_refblock(table_index)?;
        let (buf, _) = self.refblock_cache.get(table_index).expect("just cached");
        Ok(bits_get(buf, idx_in_block, self.refcount_bits))
    }

    fn refblock_set(&mut self, table_index: u64, idx_in_block: u64, value: u64) -> Result<()> {
        self.load_refblock(table_index)?;
        self.refblock_cache.mark_dirty(table_index);
        let (buf, _) = self
            .refblock_cache
            .map
            .get_mut(&table_index)
            .expect("just cached");
        bits_set(buf, idx_in_block, self.refcount_bits, value);
        Ok(())
    }

    /// Refcount of a host cluster, without allocating anything.
    pub(crate) fn get_refcount(&mut self, cluster_idx: u64) -> Result<u64> {
        let rbe = self.refblock_entries();
        self.refblock_get(cluster_idx / rbe, cluster_idx % rbe)
    }

    /// Set a cluster's refcount, allocating its refblock if needed. The
    /// free map follows the refcount: zero puts the cluster on the free
    /// list, non-zero takes it off, so the map can never hand out a
    /// cluster twice.
    pub(crate) fn set_cluster_refcount(&mut self, cluster_idx: u64, value: u64) -> Result<()> {
        let rbe = self.refblock_entries();
        let table_index = cluster_idx / rbe;
        self.ensure_refcount_table_len(table_index + 1)?;
        if self.refcount_table[table_index as usize] & OFFSET_MASK == 0 {
            self.alloc_refblock(table_index)?;
        }
        self.refblock_set(table_index, cluster_idx % rbe, value)?;
        if value == 0 {
            self.free.set.insert(cluster_idx);
        } else {
            self.free.set.remove(&cluster_idx);
        }
        Ok(())
    }

    /// Grow the in-memory refcount table (entries) so index `len` is
    /// addressable, growing the file region when the entries no longer fit.
    fn ensure_refcount_table_len(&mut self, len: u64) -> Result<()> {
        if len <= self.refcount_table.len() as u64 {
            return Ok(());
        }
        let region_clusters = (len * 8).div_ceil(self.cluster_size).max(1);
        if region_clusters > self.refcount_table_clusters as u64 {
            self.grow_refcount_table_region(region_clusters)?;
        }
        let want = (self.refcount_table_clusters as u64 * self.cluster_size / 8) as usize;
        if self.refcount_table.len() < want {
            self.refcount_table.resize(want, 0);
        }
        Ok(())
    }

    pub(crate) fn inc_refcount(&mut self, cluster_idx: u64) -> Result<()> {
        let rc = self.get_refcount(cluster_idx)?;
        if rc >= self.refcount_max() {
            return Err(Error::Unsupported(format!(
                "refcount overflow at cluster {cluster_idx}"
            )));
        }
        self.set_cluster_refcount(cluster_idx, rc + 1)
    }

    pub(crate) fn dec_refcount(&mut self, cluster_idx: u64) -> Result<()> {
        let rc = self.get_refcount(cluster_idx)?;
        if rc == 0 {
            return Err(Error::Format {
                format: "qcow2",
                reason: format!("refcount underflow at cluster {cluster_idx}"),
            });
        }
        // Hitting zero returns the cluster to the free map inside
        // set_cluster_refcount.
        self.set_cluster_refcount(cluster_idx, rc - 1)
    }

    /// Reallocate the refcount table file region to `clusters` clusters.
    /// The logical capacity (cluster count, entry vector) is committed
    /// before the new region is allocated so nested allocations during the
    /// copy see the new capacity and do not grow again.
    fn grow_refcount_table_region(&mut self, clusters: u64) -> Result<()> {
        let old_offset = self.refcount_table_offset;
        let old_clusters = self.refcount_table_clusters as u64;
        if clusters <= old_clusters {
            return Ok(());
        }
        // Snapshot the in-memory table (the disk copy may be stale) before
        // committing the new capacity.
        let old_entries = self.refcount_table.clone();
        // Commit the logical capacity first.
        self.refcount_table_clusters = clusters as u32;
        let entries = (clusters * self.cluster_size / 8) as usize;
        self.refcount_table.clear();
        self.refcount_table.resize(entries, 0);
        for (i, value) in old_entries.iter().enumerate() {
            self.refcount_table[i] = *value;
        }

        let new_offset = self.alloc_clusters_contiguous(clusters)?;
        let mut region = vec![0u8; clusters as usize * self.cluster_size as usize];
        for (i, value) in self.refcount_table.iter().enumerate() {
            region[i * 8..i * 8 + 8].copy_from_slice(&value.to_be_bytes());
        }
        self.write_file(new_offset, &region)?;
        self.refcount_table_offset = new_offset;
        self.refcount_table_dirty = true;
        self.header_dirty = true;
        for idx in 0..clusters {
            self.set_cluster_refcount(new_offset / self.cluster_size + idx, 1)?;
        }
        // Free the old region.
        if old_clusters > 0 {
            for idx in 0..old_clusters {
                self.dec_refcount(old_offset / self.cluster_size + idx)?;
            }
        }
        Ok(())
    }

    /// Allocate and install a refcount block for `table_index`.
    fn alloc_refblock(&mut self, table_index: u64) -> Result<()> {
        let rbe = self.refblock_entries();
        // A hole inside coverage hosts the block; with none it bootstraps
        // at the first cluster beyond coverage, where it can cover its own
        // cluster once the table has been extended for it.
        self.refill_free_map(false)?;
        let host_idx = match self.take_mapped_run(1) {
            Some(idx) => idx,
            None => self.refcount_table.len() as u64 * rbe,
        };
        let host = host_idx * self.cluster_size;
        self.l2_cache.remove(host);
        // Install the table entry first so refblock_set can resolve it.
        self.refcount_table[table_index as usize] = host;
        self.refcount_table_dirty = true;
        // A previous scan may have skipped this previously uncovered range.
        self.free.scan_next = self.free.scan_next.min(table_index * rbe);
        // Zero the block on disk before anything can read it.
        let zeros = vec![0u8; self.cluster_size as usize];
        self.write_file(host, &zeros)?;
        // The refblock cluster itself is referenced now. This may write into
        // the very block we just installed (when it covers its own index).
        self.set_cluster_refcount(host_idx, 1)?;
        Ok(())
    }

    /// Allocate one free cluster (rc 0 -> 1) and return its host offset.
    pub(crate) fn alloc_cluster(&mut self) -> Result<u64> {
        let idx = self.find_free(1)?;
        // A freed cluster may retain a cached L2 from its previous owner.
        // Discard it before the storage is used for a new allocation.
        self.l2_cache.remove(idx * self.cluster_size);
        self.set_cluster_refcount(idx, 1)?;
        Ok(idx * self.cluster_size)
    }

    /// Allocate `count` contiguous clusters and return the first host offset.
    pub(crate) fn alloc_clusters_contiguous(&mut self, count: u64) -> Result<u64> {
        let first = self.find_free(count)?;
        for idx in first..first + count {
            self.l2_cache.remove(idx * self.cluster_size);
            self.set_cluster_refcount(idx, 1)?;
        }
        Ok(first * self.cluster_size)
    }

    /// Find `count` contiguous free clusters, preferring holes freed
    /// earlier — by this session or a previous one — lowest first, and
    /// extending refcount coverage when coverage holds no run.
    fn find_free(&mut self, count: u64) -> Result<u64> {
        let count = count.max(1);
        loop {
            // Bounded pass first: cheap when holes sit near the scan cursor.
            self.refill_free_map(false)?;
            if let Some(first) = self.take_mapped_run(count) {
                return Ok(first);
            }
            // The bounded pass may have stopped early short of a run; a
            // full pass decides whether coverage really holds none before
            // it is grown.
            self.refill_free_map(true)?;
            if let Some(first) = self.take_mapped_run(count) {
                return Ok(first);
            }
            self.grow_coverage()?;
        }
    }

    /// Extend refcount coverage by allocating the refblock for the first
    /// table index without one, growing the table region first if needed.
    /// The new region is one large free run for the next scan pass.
    fn grow_coverage(&mut self) -> Result<()> {
        self.ensure_refcount_table_len(self.refcount_table.len() as u64 + 1)?;
        let missing = (0..self.refcount_table.len() as u64)
            .find(|ti| self.refcount_table[*ti as usize] & OFFSET_MASK == 0)
            .ok_or_else(|| {
                Error::Unsupported("no free clusters and no room to grow coverage".into())
            })?;
        self.alloc_refblock(missing)
    }

    /// Advance the lazy scan, adding free clusters of covered regions to
    /// the free map. `to_end` forces the pass to the end of coverage; a
    /// bounded pass that collects nothing has already scanned to the end,
    /// so an empty map afterwards means coverage really is full.
    fn refill_free_map(&mut self, to_end: bool) -> Result<()> {
        let rbe = self.refblock_entries();
        let covered = self.refcount_table.len() as u64 * rbe;
        while self.free.scan_next < covered {
            let ti = self.free.scan_next / rbe;
            if self
                .refcount_table
                .get(ti as usize)
                .is_none_or(|e| e & OFFSET_MASK == 0)
            {
                // No refblock: the region reads as free but is not usable
                // without coverage, which the growth path allocates.
                self.free.scan_next = (ti + 1) * rbe;
                continue;
            }
            self.load_refblock(ti)?;
            let buf = {
                let (b, _) = self.refblock_cache.get(ti).expect("loaded");
                b.clone()
            };
            for i in self.free.scan_next % rbe..rbe {
                if bits_get(&buf, i, self.refcount_bits) == 0 {
                    self.free.set.insert(ti * rbe + i);
                }
                self.free.scan_next = ti * rbe + i + 1;
                if !to_end && self.free.set.len() >= FREE_SCAN_TARGET {
                    return Ok(());
                }
            }
            self.free.scan_next = (ti + 1) * rbe;
            if !to_end && self.free.set.len() >= FREE_SCAN_TARGET {
                break;
            }
        }
        Ok(())
    }

    /// Take the lowest run of `count` adjacent free clusters out of the
    /// free map.
    fn take_mapped_run(&mut self, count: u64) -> Option<u64> {
        let count = count.max(1);
        let mut start = 0u64;
        let mut len = 0u64;
        let mut found = None;
        for &c in &self.free.set {
            if len > 0 && c == start + len {
                len += 1;
            } else {
                start = c;
                len = 1;
            }
            if len == count {
                found = Some(start);
                break;
            }
        }
        let first = found?;
        for idx in first..first + count {
            self.free.set.remove(&idx);
        }
        Some(first)
    }

    /// Ensure the on-disk L1 region holds at least `needed` entries and the
    /// in-memory view matches.
    pub(crate) fn ensure_l1_size(&mut self, needed: u32) -> Result<()> {
        if needed <= self.l1_size {
            if self.l1.len() > needed as usize {
                // Zero the stale tail on disk.
                let start = needed as u64 * 8;
                let end = self.l1.len() as u64 * 8;
                let region = self.l1_table_offset + start;
                let zeros = vec![0u8; (end - start) as usize];
                self.write_file(region, &zeros)?;
                self.l1.truncate(needed as usize);
                self.l1_size = needed;
                self.l1_dirty = true;
                self.header_dirty = true;
            }
            return Ok(());
        }
        let old_clusters = (self.l1_size as u64 * 8).div_ceil(self.cluster_size).max(1);
        let new_clusters = (needed as u64 * 8).div_ceil(self.cluster_size).max(1);
        self.l1.resize(needed as usize, 0);
        if new_clusters > old_clusters {
            let new_offset = self.alloc_clusters_contiguous(new_clusters)?;
            let mut region = vec![0u8; new_clusters as usize * self.cluster_size as usize];
            for (i, entry) in self.l1.iter().enumerate() {
                region[i * 8..i * 8 + 8].copy_from_slice(&entry.to_be_bytes());
            }
            self.write_file(new_offset, &region)?;
            for idx in 0..new_clusters {
                self.set_cluster_refcount(new_offset / self.cluster_size + idx, 1)?;
            }
            for idx in 0..old_clusters {
                self.dec_refcount(self.l1_table_offset / self.cluster_size + idx)?;
            }
            self.l1_table_offset = new_offset;
        } else {
            // Region big enough; zero the newly covered entries.
            let start = self.l1_size as u64 * 8;
            let end = needed as u64 * 8;
            let zeros = vec![0u8; (end - start) as usize];
            self.write_file(self.l1_table_offset + start, &zeros)?;
        }
        self.l1_size = needed;
        self.l1_dirty = true;
        self.header_dirty = true;
        Ok(())
    }

    /// Adjust the refcount of every cluster referenced by an L1 table
    /// (the L2 tables it points at and all clusters those map, including
    /// compressed-cluster byte ranges).
    pub(crate) fn walk_refcounts(&mut self, l1: &[u64], delta: i64) -> Result<()> {
        for &l1e in l1 {
            let l2_host = l1e & OFFSET_MASK;
            if l2_host == 0 {
                continue;
            }
            self.adjust_one(l2_host / self.cluster_size, delta)?;
            self.load_l2(l2_host)?;
            let entries: Vec<u64> = {
                let (buf, _) = self.l2_cache.get(l2_host).expect("cached");
                buf.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| u64::from_be_bytes(*c))
                    .collect()
            };
            for entry in entries {
                if entry & super::COMPRESSED_BIT != 0 {
                    self.adjust_compressed_refcounts(entry, delta)?;
                } else {
                    let host = entry & OFFSET_MASK;
                    if host != 0 {
                        self.adjust_one(host / self.cluster_size, delta)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn adjust_one(&mut self, cluster_idx: u64, delta: i64) -> Result<()> {
        if delta >= 0 {
            self.inc_refcount(cluster_idx)
        } else {
            self.dec_refcount(cluster_idx)
        }
    }
}
