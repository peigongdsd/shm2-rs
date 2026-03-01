use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub struct Allocation {
    pub buffer_id: u32,
    pub offset: u64,
    pub len: u32,
}

#[derive(Debug)]
pub struct FreeListAllocator {
    free: Vec<(u64, u64)>,
    used: BTreeMap<u32, (u64, u64)>,
    next_id: u32,
}

impl FreeListAllocator {
    pub fn new(total_size: u64) -> Self {
        Self {
            free: vec![(0, total_size)],
            used: BTreeMap::new(),
            next_id: 1,
        }
    }

    pub fn alloc(&mut self, size: u32, align: u64) -> Option<Allocation> {
        let size = u64::from(size);
        let align = align.max(1);

        let mut idx = 0;
        while idx < self.free.len() {
            let (start, len) = self.free[idx];
            let aligned_start = align_up(start, align);
            let end = start + len;

            if aligned_start + size <= end {
                let left_len = aligned_start.saturating_sub(start);
                let right_start = aligned_start + size;
                let right_len = end.saturating_sub(right_start);

                self.free.remove(idx);
                if right_len > 0 {
                    self.free.insert(idx, (right_start, right_len));
                }
                if left_len > 0 {
                    self.free.insert(idx, (start, left_len));
                }

                let buffer_id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1).max(1);
                self.used.insert(buffer_id, (aligned_start, size));

                return Some(Allocation {
                    buffer_id,
                    offset: aligned_start,
                    len: size as u32,
                });
            }
            idx += 1;
        }

        None
    }

    pub fn free_by_id(&mut self, buffer_id: u32) -> bool {
        if let Some((start, len)) = self.used.remove(&buffer_id) {
            self.insert_and_coalesce(start, len);
            true
        } else {
            false
        }
    }

    pub fn used_bytes(&self) -> u64 {
        self.used.values().map(|(_, len)| *len).sum()
    }

    fn insert_and_coalesce(&mut self, start: u64, len: u64) {
        self.free.push((start, len));
        self.free.sort_by_key(|(s, _)| *s);

        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.free.len());
        for (s, l) in self.free.drain(..) {
            if let Some((last_s, last_l)) = merged.last_mut() {
                let last_end = *last_s + *last_l;
                if s <= last_end {
                    let end = (s + l).max(last_end);
                    *last_l = end - *last_s;
                    continue;
                }
            }
            merged.push((s, l));
        }
        self.free = merged;
    }
}

fn align_up(v: u64, align: u64) -> u64 {
    if align <= 1 {
        return v;
    }
    let rem = v % align;
    if rem == 0 { v } else { v + (align - rem) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_and_free_coalesces() {
        let mut a = FreeListAllocator::new(1024);
        let b1 = a.alloc(128, 64).unwrap();
        let b2 = a.alloc(128, 64).unwrap();
        assert!(a.free_by_id(b1.buffer_id));
        assert!(a.free_by_id(b2.buffer_id));
        assert_eq!(a.used_bytes(), 0);
        let b3 = a.alloc(1024, 1).unwrap();
        assert_eq!(b3.offset, 0);
        assert_eq!(b3.len, 1024);
    }
}
