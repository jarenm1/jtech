//! Body-driven residency for the sparse GPU terrain, independent of player interest.
use glam::{IVec3, Vec3};
use gpu_physics::{Body, GpuPhysics, MAX_TERRAIN_CHUNKS, TERRAIN_CHUNK_SIZE};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use voxel_world::{CHUNK_SIZE, CHUNK_VOLUME, Chunk, MAX_CHUNK_Y, MIN_CHUNK_Y, VoxelWorld};

// A 50 ms batch integrates at most 30 * .05 = 1.5 m. Two projected
// corrections of .25 m per axis, four iterations, six substeps add 12 m.
// Including the cube's .5 m half extent gives 14 m; round up to 16 m.
const HALO: f64 = 16.0;
const MAX_COORD: i32 = 1_000_000;
const MAX_POSITION: f32 = (MAX_COORD * CHUNK_SIZE) as f32;

#[derive(Default)]
pub(super) struct ActiveTerrain {
    // Holding the old Arc makes authoritative edits copy-on-write. Identity
    // also detects a replacement chunk with an unchanged revision number.
    snapshots: HashMap<IVec3, Arc<Chunk>>,
}

fn valid_position(body: &Body) -> bool {
    let position = Vec3::from_array(body.position);
    position.is_finite() && position.abs().max_element() <= MAX_POSITION
}

/// Conservative batch collision footprint. Each enabled body needs at most
/// eight chunks; invalid coordinates are ignored here and rejected by sync.
/// Only the world's finite vertical terrain range requires resident pages.
pub(super) fn needed_chunks(bodies: &[Body]) -> HashSet<IVec3> {
    let mut needed = HashSet::new();
    for body in bodies
        .iter()
        .filter(|body| body.material != 0 && valid_position(body))
    {
        // Use f64 for the halo arithmetic: at large f32 coordinates, rounding
        // the bounds must not silently shrink the collision envelope.
        let low = body
            .position
            .map(|p| ((f64::from(p) - HALO) / f64::from(CHUNK_SIZE)).floor() as i32);
        let high = body
            .position
            .map(|p| ((f64::from(p) + HALO) / f64::from(CHUNK_SIZE)).floor() as i32);
        for y in low[1].max(MIN_CHUNK_Y)..=high[1].min(MAX_CHUNK_Y) {
            for z in low[2].max(-MAX_COORD)..=high[2].min(MAX_COORD) {
                for x in low[0].max(-MAX_COORD)..=high[0].min(MAX_COORD) {
                    needed.insert(IVec3::new(x, y, z));
                }
            }
        }
    }
    needed
}

fn decode(chunk: &Chunk) -> Vec<u32> {
    let mut cells = Vec::with_capacity(CHUNK_VOLUME);
    for y in 0..CHUNK_SIZE {
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                cells.push(u32::from(chunk.get(IVec3::new(x, y, z))));
            }
        }
    }
    cells
}

impl ActiveTerrain {
    /// Synchronize only while idle. Missing world chunks postpone the entire
    /// batch, without uploading partial terrain or changing the cache.
    /// Return actual GPU upload bytes, including page-table updates.
    pub fn sync(
        &mut self,
        world: &VoxelWorld,
        bodies: &[Body],
        gpu: &mut GpuPhysics,
    ) -> Result<Option<u64>, String> {
        if gpu.is_busy() {
            return Err("terrain residency update during physics submission".into());
        }
        if bodies
            .iter()
            .any(|body| body.material != 0 && !valid_position(body))
        {
            return Err("body outside supported terrain coordinates".into());
        }
        debug_assert_eq!(CHUNK_SIZE as u32, TERRAIN_CHUNK_SIZE);
        let needed = needed_chunks(bodies);
        if needed.len() > MAX_TERRAIN_CHUNKS {
            return Err(format!(
                "physics terrain needs {} pages; limit is {MAX_TERRAIN_CHUNKS}",
                needed.len()
            ));
        }
        if needed.iter().any(|coord| !world.chunks.contains_key(coord)) {
            return Ok(None);
        }
        let mut retained: Vec<_> = needed.iter().map(|coord| coord.to_array()).collect();
        retained.sort_unstable();
        let changed: Vec<_> = retained
            .iter()
            .filter_map(|&coord| {
                let key = IVec3::from_array(coord);
                let chunk = &world.chunks[&key];
                if self
                    .snapshots
                    .get(&key)
                    .is_some_and(|old| Arc::ptr_eq(old, chunk))
                {
                    None
                } else {
                    Some((coord, decode(chunk)))
                }
            })
            .collect();
        if changed.is_empty() && needed.len() == self.snapshots.len() {
            return Ok(Some(0));
        }
        let uploaded = gpu.update_sparse_terrain(&retained, &changed)?;
        // Commit only after a successful upload. Retaining unchanged snapshots
        // avoids decoding their cells or reallocating their GPU pages.
        self.snapshots.retain(|coord, _| needed.contains(coord));
        for (coord, _) in changed {
            let coord = IVec3::from_array(coord);
            self.snapshots
                .insert(coord, Arc::clone(&world.chunks[&coord]));
        }
        Ok(Some(uploaded))
    }

    pub fn resident_count(&self) -> usize {
        self.snapshots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_positive_and_negative_bodies_need_only_local_pages() {
        for (x, z, cx, cz) in [(3200., 6400., 100, 200), (-3200., -6400., -100, -200)] {
            let chunks = needed_chunks(&[Body::new([x, 16., z], 3)]);
            let expected: HashSet<_> = [0, 1]
                .into_iter()
                .flat_map(|y| {
                    [cz - 1, cz].into_iter().flat_map(move |z| {
                        [cx - 1, cx].into_iter().map(move |x| IVec3::new(x, y, z))
                    })
                })
                .collect();
            assert_eq!(chunks, expected);
        }
    }

    #[test]
    fn seam_crossing_retains_overlap_and_releases_trailing_pages() {
        let before = needed_chunks(&[Body::new([15.5, 16., 16.], 3)]);
        let after = needed_chunks(&[Body::new([16.5, 16., 16.], 3)]);
        assert!(before.iter().any(|p| p.x == -1));
        assert!(after.iter().any(|p| p.x == 1));
        assert!(after.iter().all(|p| p.x >= 0));
        assert_eq!(before.intersection(&after).count(), 4);
    }

    #[test]
    fn batch_projection_envelope_has_no_residency_gaps() {
        // Include negative seams and large coordinates, where f32 precision
        // could otherwise narrow the halo during bound calculation.
        for x in [
            -31_999_984.,
            -48.,
            -32.,
            -16.,
            -0.5,
            0.,
            15.5,
            16.,
            32.,
            31_999_984.,
        ] {
            let body = Body::new([x, 16., x], 3);
            let chunks = needed_chunks(&[body]);
            for dx in -14..=14 {
                for dy in -14..=14 {
                    for dz in -14..=14 {
                        let cell = IVec3::new(
                            (f64::from(x) + f64::from(dx)).floor() as i32,
                            16 + dy,
                            (f64::from(x) + f64::from(dz)).floor() as i32,
                        );
                        assert!(chunks.contains(&voxel_world::chunk_coord(cell)));
                    }
                }
            }
            assert!(chunks.len() <= 8);
        }
    }

    #[test]
    fn footprints_are_bounded_clipped_and_ignore_disabled_or_invalid_bodies() {
        let mut bodies = vec![Body::default()];
        for position in [
            [f32::NAN, 0., 0.],
            [f32::INFINITY, 0., 0.],
            [f32::MAX, 0., 0.],
        ] {
            bodies.push(Body::new(position, 3));
        }
        assert!(needed_chunks(&bodies).is_empty());
        assert!(needed_chunks(&[Body::new([0., 1000., 0.], 3)]).is_empty());
        let bodies: Vec<_> = (0..256)
            .map(|i| Body::new([i as f32 * 128., -31., 0.], 3))
            .collect();
        let chunks = needed_chunks(&bodies);
        assert!(chunks.len() <= bodies.len() * 8);
        assert!(chunks.len() <= MAX_TERRAIN_CHUNKS);
        assert!(
            chunks
                .iter()
                .all(|coord| (MIN_CHUNK_Y..=MAX_CHUNK_Y).contains(&coord.y))
        );
    }

    #[test]
    fn decoding_uses_gpu_x_z_y_material_order() {
        let mut world = VoxelWorld::default();
        world.insert(
            IVec3::ZERO,
            Chunk::from_runs(7, &[(CHUNK_VOLUME as u16, 0)]).unwrap(),
        );
        for (cell, material) in [
            (IVec3::X, 1),
            (IVec3::Z, 2),
            (IVec3::Y, 3),
            (IVec3::splat(31), 5),
        ] {
            world.set_block(cell, material).unwrap();
        }
        let cells = decode(&world.chunks[&IVec3::ZERO]);
        assert_eq!(cells.len(), CHUNK_VOLUME);
        assert_eq!(cells[1], 1);
        assert_eq!(cells[32], 2);
        assert_eq!(cells[1024], 3);
        assert_eq!(cells[CHUNK_VOLUME - 1], 5);
        assert_eq!(cells.iter().filter(|&&material| material != 0).count(), 4);
    }
}
