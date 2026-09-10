//! CPU page allocation and validated, bounded upload plans. No GPU state is touched here.
use std::collections::{BTreeMap, BTreeSet};

pub const TERRAIN_CHUNK_SIZE: u32 = 32;
pub const MAX_TERRAIN_CHUNKS: usize = 4096;
pub(crate) const CHUNK_CELLS: usize = (TERRAIN_CHUNK_SIZE as usize).pow(3);
pub(crate) const PAGE_WORDS: usize = CHUNK_CELLS / 8;
pub(crate) const TABLE_CAPACITY: usize = MAX_TERRAIN_CHUNKS * 2;
pub(crate) const TABLE_WORDS: usize = TABLE_CAPACITY * 4;
pub(crate) const BUFFER_BYTES: u64 = ((TABLE_WORDS + MAX_TERRAIN_CHUNKS * PAGE_WORDS) * 4) as u64;
pub(crate) const BROADPHASE_BUCKETS: u32 = 1024;
type Coord = [i32; 3];

/// Identical wrapping hash in broadphase.wgsl. Signed coordinates retain their bits.
pub(crate) fn spatial_hash(c: Coord) -> u32 {
    (c[0] as u32).wrapping_mul(73856093)
        ^ (c[1] as u32).wrapping_mul(19349663)
        ^ (c[2] as u32).wrapping_mul(83492791)
}

#[derive(Default)]
pub(crate) struct SparseTerrain {
    slots: BTreeMap<Coord, usize>,
}
pub(crate) struct UploadPlan {
    pub next: SparseTerrain,
    pub table: Option<Vec<u32>>,
    pub pages: Vec<(usize, Vec<u32>)>,
}
impl SparseTerrain {
    /// Retained is the complete desired set, not just unchanged coordinates.
    pub fn plan(
        &self,
        retained: &[Coord],
        changed: &[(Coord, Vec<u32>)],
    ) -> Result<UploadPlan, String> {
        if retained.len() > MAX_TERRAIN_CHUNKS || changed.len() > MAX_TERRAIN_CHUNKS {
            return Err("sparse terrain capacity exceeded".into());
        }
        let desired: BTreeSet<_> = retained.iter().copied().collect();
        if desired.len() != retained.len() {
            return Err("duplicate resident terrain coordinate".into());
        }
        let mut replacements = BTreeSet::new();
        for (coord, cells) in changed {
            if !desired.contains(coord) || !replacements.insert(*coord) {
                return Err("changed terrain coordinates must be unique and resident".into());
            }
            if cells.len() != CHUNK_CELLS || cells.iter().any(|&m| m > 5) {
                return Err("terrain chunks require 32768 materials in 0..=5".into());
            }
        }
        if desired
            .iter()
            .any(|c| !self.slots.contains_key(c) && !replacements.contains(c))
        {
            return Err("new resident terrain chunk requires cell data".into());
        }
        let mut slots: BTreeMap<_, _> = self
            .slots
            .iter()
            .filter(|(c, _)| desired.contains(*c))
            .map(|(&c, &s)| (c, s))
            .collect();
        let used: BTreeSet<_> = slots.values().copied().collect();
        let mut free = (0..MAX_TERRAIN_CHUNKS).filter(|s| !used.contains(s));
        for coord in desired {
            slots
                .entry(coord)
                .or_insert_with(|| free.next().expect("validated page capacity"));
        }
        let table = if slots == self.slots {
            None
        } else {
            let mut words = vec![0; TABLE_WORDS];
            for (&coord, &slot) in &slots {
                let mut bucket = spatial_hash(coord) as usize % TABLE_CAPACITY;
                while words[bucket * 4 + 3] != 0 {
                    bucket = (bucket + 1) % TABLE_CAPACITY;
                }
                words[bucket * 4..bucket * 4 + 3].copy_from_slice(&coord.map(|v| v as u32));
                words[bucket * 4 + 3] = slot as u32 + 1;
            }
            Some(words)
        };
        let pages = changed
            .iter()
            .map(|(coord, cells)| {
                let packed = cells
                    .chunks_exact(8)
                    .map(|eight| {
                        eight
                            .iter()
                            .enumerate()
                            .fold(0, |word, (i, &m)| word | (m << (i * 4)))
                    })
                    .collect();
                (slots[coord], packed)
            })
            .collect();
        Ok(UploadPlan {
            next: Self { slots },
            table,
            pages,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_before_allocating_and_reuses_pages() {
        let empty = SparseTerrain::default();
        assert!(empty.plan(&[[0; 3]], &[]).is_err());
        assert!(empty.plan(&[[0; 3]; 2], &[]).is_err());
        assert!(empty.plan(&[], &[([0; 3], vec![0; CHUNK_CELLS])]).is_err());
        assert!(
            empty
                .plan(&[[0; 3]], &[([0; 3], vec![6; CHUNK_CELLS])])
                .is_err()
        );
        assert!(empty.plan(&[[0; 3]], &[([0; 3], vec![0; 8])]).is_err());
        assert!(
            empty
                .plan(&vec![[0; 3]; MAX_TERRAIN_CHUNKS + 1], &[])
                .is_err()
        );
        let coords = [[-900, 2, 44], [900, -2, -44]];
        let first = empty
            .plan(&coords, &coords.map(|c| (c, vec![3; CHUNK_CELLS])))
            .unwrap();
        assert_eq!(first.pages[0].1, vec![0x33333333; PAGE_WORDS]);
        let retained_slot = first.next.slots[&coords[1]];
        let fresh = [1, 2, 3];
        let next = first
            .next
            .plan(&[coords[1], fresh], &[(fresh, vec![5; CHUNK_CELLS])])
            .unwrap();
        assert_eq!(next.next.slots[&coords[1]], retained_slot);
        assert_eq!(next.next.slots[&fresh], first.next.slots[&coords[0]]);
        assert!(
            next.next
                .plan(&[coords[1], fresh], &[])
                .unwrap()
                .table
                .is_none()
        );
        assert_eq!(empty.slots.len(), 0);
        const { assert!(BUFFER_BYTES < 128 * 1024 * 1024) };
    }
    #[test]
    fn packed_index_order_and_colliding_table_entries() {
        let a = [-12, 7, 3];
        let b = (1..100000)
            .map(|x| [x, 7, 3])
            .find(|&c| {
                spatial_hash(c) % TABLE_CAPACITY as u32 == spatial_hash(a) % TABLE_CAPACITY as u32
            })
            .unwrap();
        let mut cells = vec![0; CHUNK_CELLS];
        for (i, m) in cells.iter_mut().enumerate() {
            *m = (i % 6) as u32;
        }
        let plan = SparseTerrain::default()
            .plan(&[a, b], &[(a, cells.clone()), (b, cells.clone())])
            .unwrap();
        let table = plan.table.unwrap();
        let bucket = spatial_hash(a) as usize % TABLE_CAPACITY;
        assert_ne!(table[bucket * 4 + 3], 0);
        assert_ne!(table[((bucket + 1) % TABLE_CAPACITY) * 4 + 3], 0);
        for (_, page) in plan.pages {
            for (i, &m) in cells.iter().enumerate() {
                assert_eq!((page[i / 8] >> ((i % 8) * 4)) & 15, m);
            }
        }
    }
}
