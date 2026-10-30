use super::*;
use virtio_queue::QueueState;

impl Snapshot for Gpu {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let s = self.state.lock();
        let mut out = b"GPU2\x01\0\0\0".to_vec();
        out.extend_from_slice(&s.acked.to_le_bytes());
        put(
            &mut out,
            &[s.preferred.0, s.preferred.1, s.events, s.native as u32],
        );
        if let Some((id, r)) = s.scanout {
            put(&mut out, &[id, r.x, r.y, r.w, r.h]);
        } else {
            put(&mut out, &[0; 5]);
        }
        put(&mut out, &[s.resources.len() as u32]);
        for (&id, r) in &s.resources {
            put(
                &mut out,
                &[id, r.width, r.height, r.format, r.backing.len() as u32],
            );
            for &(addr, len) in &r.backing {
                out.extend_from_slice(&addr.to_le_bytes());
                put(&mut out, &[len]);
            }
            out.extend_from_slice(&r.pixels);
        }
        let c = &s.cursor;
        put(
            &mut out,
            &[
                c.width,
                c.height,
                c.hot_x,
                c.hot_y,
                c.x,
                c.y,
                c.visible as u32,
            ],
        );
        out.extend_from_slice(&c.generation.to_le_bytes());
        put(&mut out, &[c.pixels.len() as u32]);
        out.extend_from_slice(&c.pixels);
        let queues = if s.queues.is_empty() {
            &s.pending
        } else {
            &s.queues
        };
        put(&mut out, &[queues.len() as u32]);
        for q in queues {
            let q = q.state();
            put(
                &mut out,
                &[
                    q.max_size as u32,
                    q.size as u32,
                    q.next_avail as u32,
                    q.next_used as u32,
                    q.event_idx_enabled as u32,
                    q.ready as u32,
                ],
            );
            for addr in [q.desc_table, q.avail_ring, q.used_ring] {
                out.extend_from_slice(&addr.to_le_bytes());
            }
        }
        Ok(out)
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        self.restore(state)
            .map_err(|_| DeviceError::State("invalid virtio-gpu snapshot".into()))
    }
}

fn put(out: &mut Vec<u8>, values: &[u32]) {
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

impl Gpu {
    fn restore(&mut self, state: &[u8]) -> Result<(), u32> {
        let mut d = Decoder::new(state);
        if d.take(8)? != b"GPU2\x01\0\0\0" {
            return Err(INVALID);
        }
        let acked = d.u64()?;
        if acked & !FEATURES != 0 {
            return Err(INVALID);
        }
        let preferred = (d.u32()?, d.u32()?);
        if !valid_size(preferred.0, preferred.1) {
            return Err(INVALID);
        }
        let events = d.u32()?;
        let native = d.u32()?;
        if native > 1 || events & !1 != 0 {
            return Err(INVALID);
        }
        let scanout_id = d.u32()?;
        let rect = d.rect()?;
        let n = d.u32()? as usize;
        if n > MAX_RESOURCES {
            return Err(INVALID);
        }
        let mut resources = BTreeMap::new();
        let mut total = 0;
        for _ in 0..n {
            let id = d.u32()?;
            let width = d.u32()?;
            let height = d.u32()?;
            let format = d.u32()?;
            let n = d.u32()? as usize;
            if id == 0
                || resources.contains_key(&id)
                || !valid_size(width, height)
                || !valid_format(format)
                || n > MAX_ENTRIES
            {
                return Err(INVALID);
            }
            let len = width as usize * height as usize * 4;
            total += len;
            if total > MAX_BYTES {
                return Err(INVALID);
            }
            let mut backing = Vec::with_capacity(n);
            for _ in 0..n {
                let addr = d.u64()?;
                let len = d.u32()?;
                if len == 0 || addr.checked_add(u64::from(len)).is_none() {
                    return Err(INVALID);
                }
                backing.push((addr, len));
            }
            resources.insert(
                id,
                Resource {
                    width,
                    height,
                    format,
                    backing,
                    pixels: d.take(len)?.to_vec(),
                },
            );
        }
        let scanout = if scanout_id == 0 {
            None
        } else {
            let r = resources.get(&scanout_id).ok_or(INVALID)?;
            if native == 0 || !rect.fits(r.width, r.height) {
                return Err(INVALID);
            }
            Some((scanout_id, rect))
        };
        let mut cursor = Cursor {
            width: d.u32()?,
            height: d.u32()?,
            hot_x: d.u32()?,
            hot_y: d.u32()?,
            x: d.u32()?,
            y: d.u32()?,
            ..Cursor::default()
        };
        let visible = d.u32()?;
        cursor.generation = d.u64()?;
        let len = d.u32()? as usize;
        if visible > 1
            || !matches!(
                (cursor.width, cursor.height, len),
                (0, 0, 0) | (64, 64, 16384)
            )
            || (visible == 1 && len == 0)
            || cursor.hot_x >= 64
            || cursor.hot_y >= 64
        {
            return Err(INVALID);
        }
        cursor.visible = visible == 1;
        cursor.pixels = d.take(len)?.to_vec();
        let n = d.u32()? as usize;
        if n != 0 && n != 2 {
            return Err(INVALID);
        }
        let mut queues = Vec::with_capacity(n);
        for i in 0..n {
            let max_size = d.u32()?;
            let size = d.u32()?;
            let next_avail = d.u32()?;
            let next_used = d.u32()?;
            let event = d.u32()?;
            let ready = d.u32()?;
            if max_size != [256, 64][i]
                || size > max_size
                || next_avail > 65535
                || next_used > 65535
                || event > 1
                || ready > 1
            {
                return Err(INVALID);
            }
            let q = Queue::try_from(QueueState {
                max_size: max_size as u16,
                size: size as u16,
                next_avail: next_avail as u16,
                next_used: next_used as u16,
                event_idx_enabled: event == 1,
                ready: ready == 1,
                desc_table: d.u64()?,
                avail_ring: d.u64()?,
                used_ring: d.u64()?,
            })
            .map_err(|_| INVALID)?;
            queues.push(q);
        }
        if !d.bytes.is_empty() {
            return Err(INVALID);
        }
        let mut s = self.state.lock();
        if let Some(mem) = &s.mem
            && (queues.iter().any(|q| q.ready() && !q.is_valid(mem))
                || resources.values().any(|r| {
                    r.backing.iter().any(|&(a, n)| {
                        !mem.check_range(GuestAddress(a), n as usize, Permissions::Read)
                    })
                }))
        {
            return Err(INVALID);
        }
        cursor.generation = cursor.generation.max(s.cursor.generation).wrapping_add(1);
        s.acked = acked;
        s.preferred = preferred;
        s.events = events;
        s.native = native == 1;
        s.scanout = scanout;
        s.resources = resources;
        s.cursor = cursor;
        s.dirty = true;
        if s.mem.is_some() {
            s.queues = queues;
        } else {
            s.pending = queues;
        }
        Ok(())
    }
}
