//! qcow2 host-cluster refcounts and the free-cluster allocator.
//!
//! Invariants kept for `qemu-img check` to pass:
//! - every cluster referenced by any structure (header cluster, refcount
//!   table, refblocks, active and snapshot L1 tables, L2 tables, data
//!   clusters, compressed-cluster byte ranges, snapshot table) carries a
//!   matching refcount;
//! - metadata regions (L1, refcount table, snapshot L1, snapshot table) are
//!   contiguous runs of whole clusters.

use super::{OFFSET_MASK, Qcow2Image};
use crate::{Error, Result};

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
        self.refblock_cache.put(table_index, buf, false);
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

    /// Like [`Self::get_refcount`] but for the free-hint scan, where a
    /// missing refblock is simply an all-zero region.
    fn try_get_refcount(&mut self, cluster_idx: u64) -> u64 {
        self.get_refcount(cluster_idx).unwrap_or(0)
    }

    /// Set a cluster's refcount, allocating its refblock if needed.
    pub(crate) fn set_cluster_refcount(&mut self, cluster_idx: u64, value: u64) -> Result<()> {
        let rbe = self.refblock_entries();
        let table_index = cluster_idx / rbe;
        self.ensure_refcount_table_len(table_index + 1)?;
        if self.refcount_table[table_index as usize] & OFFSET_MASK == 0 {
            self.alloc_refblock(table_index)?;
        }
        self.refblock_set(table_index, cluster_idx % rbe, value)
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
        self.set_cluster_refcount(cluster_idx, rc - 1)?;
        if rc == 1 {
            // Free clusters go back on the front of the search.
            self.free_hint = self.free_hint.min(cluster_idx);
        }
        Ok(())
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
        // Where the refblock would like to live if coverage is full: the
        // first cluster beyond current coverage.
        let beyond = self.refcount_table.len() as u64 * rbe;
        let host_idx = match self.find_free_in_coverage(1) {
            Some(idx) => idx,
            None => beyond, // bootstrap: the block may cover itself below
        };
        let host = host_idx * self.cluster_size;
        // Install the table entry first so refblock_set can resolve it.
        self.refcount_table[table_index as usize] = host;
        self.refcount_table_dirty = true;
        // Zero the block on disk before anything can read it.
        let zeros = vec![0u8; self.cluster_size as usize];
        self.write_file(host, &zeros)?;
        // The refblock cluster itself is referenced now. This may write into
        // the very block we just installed (when it covers its own index).
        self.set_cluster_refcount(host_idx, 1)?;
        Ok(())
    }

    /// First free cluster strictly inside existing refcount coverage,
    /// starting at the free hint. Never allocates coverage.
    fn find_free_in_coverage(&mut self, count: u64) -> Option<u64> {
        let rbe = self.refblock_entries();
        let covered = self.refcount_table.len() as u64 * rbe;
        let mut idx = self.free_hint;
        'outer: while idx + count <= covered {
            let ti = idx / rbe;
            let start = idx % rbe;
            if self
                .refcount_table
                .get(ti as usize)
                .is_none_or(|e| e & OFFSET_MASK == 0)
            {
                // No refblock: the whole region reads as zero (free), but
                // taking from it would need coverage allocation which this
                // helper must not do.
                idx = (ti + 1) * rbe;
                continue 'outer;
            }
            let run = self.scan_refblock_run(ti, start, count);
            match run {
                Some(hit) => return Some(hit),
                None => {
                    idx = (ti + 1) * rbe;
                }
            }
        }
        None
    }

    /// Find a run of `count` zero entries in refblock `ti` starting at
    /// `start_in_block`; returns the absolute cluster index.
    fn scan_refblock_run(&mut self, ti: u64, start_in_block: u64, count: u64) -> Option<u64> {
        let rbe = self.refblock_entries();
        if start_in_block + count > rbe {
            return None;
        }
        if self.refblock_get(ti, start_in_block).is_err() {
            return None;
        }
        let mut run = 0u64;
        let mut idx = start_in_block;
        while idx < rbe {
            if self.try_get_refcount(ti * rbe + idx) == 0 {
                run += 1;
                if run >= count {
                    return Some(ti * rbe + idx + 1 - count);
                }
            } else {
                run = 0;
            }
            idx += 1;
        }
        None
    }

    /// Allocate one free cluster (rc 0 -> 1) and return its host offset.
    pub(crate) fn alloc_cluster(&mut self) -> Result<u64> {
        let idx = self.find_free(1)?;
        self.set_cluster_refcount(idx, 1)?;
        self.free_hint = idx + 1;
        Ok(idx * self.cluster_size)
    }

    /// Allocate `count` contiguous clusters and return the first host offset.
    pub(crate) fn alloc_clusters_contiguous(&mut self, count: u64) -> Result<u64> {
        let first = self.find_free(count)?;
        for idx in first..first + count {
            self.set_cluster_refcount(idx, 1)?;
        }
        self.free_hint = first + count;
        Ok(first * self.cluster_size)
    }

    /// Find `count` contiguous free clusters, extending refcount coverage as
    /// needed. Refblocks allocated along the way never break a candidate run
    /// (the search restarts past them).
    fn find_free(&mut self, count: u64) -> Result<u64> {
        loop {
            if let Some(hit) = self.find_free_in_coverage(count) {
                return Ok(hit);
            }
            // Extend coverage: allocate a refblock for the first table index
            // that has none. If the table itself is exhausted, grow it.
            let rbe = self.refblock_entries();
            let next_region = self.refcount_table.len() as u64; // table entries are dense
            self.ensure_refcount_table_len(next_region + 1)?;
            let missing = (0..self.refcount_table.len() as u64)
                .find(|ti| self.refcount_table[*ti as usize] & OFFSET_MASK == 0)
                .ok_or_else(|| {
                    Error::Unsupported("no free clusters and no room to grow coverage".into())
                })?;
            self.alloc_refblock(missing)?;
            let _ = rbe;
        }
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

    /// Rewrite the free hint from the refcount structures (used at open).
    pub(crate) fn recompute_free_hint_from_tables(&mut self) {
        let rbe = self.refblock_entries();
        let width = self.refcount_bits;
        let indexes: Vec<u64> = (0..self.refcount_table.len() as u64)
            .filter(|ti| self.refcount_table[*ti as usize] & OFFSET_MASK != 0)
            .collect();
        let mut hint = 1u64;
        for ti in indexes {
            if self.load_refblock(ti).is_err() {
                continue;
            }
            let buf = {
                let (b, _) = self.refblock_cache.get(ti).expect("loaded");
                b.clone()
            };
            for i in 0..rbe {
                if bits_get(&buf, i, width) > 0 {
                    hint = ti * rbe + i + 1;
                }
            }
        }
        self.free_hint = hint;
    }
}
