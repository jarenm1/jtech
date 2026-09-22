//! Native generation and column surveys run outside the simulation tick.
use std::{
    collections::{HashMap, HashSet},
    io,
    sync::{Arc, mpsc},
};

use glam::{IVec2, IVec3, Vec3};
use parking_lot::Mutex;
use voxel_world::{
    CHUNK_SIZE, Chunk, MAX_CHUNK_Y, MIN_CHUNK_Y, chunk_coord, terrain::TerrainGenerator,
};

use crate::{streaming::nearest_chunks, valid_coord};

const MAX_PENDING: usize = 16;
// Reserve survey capacity even while chunk generation has a continuous backlog.
const MAX_PENDING_CHUNKS: usize = 12;
const SURVEYS_PER_TICK: usize = 8;

enum Work {
    Survey(IVec2),
    Generate(IVec3),
}
enum Completed {
    Survey(IVec2, (i32, i32)),
    Generate(IVec3, Chunk),
}

pub(super) struct TerrainStream {
    requests: mpsc::SyncSender<Work>,
    results: Mutex<mpsc::Receiver<Completed>>,
    pending_columns: HashSet<IVec2>,
    pending_chunks: HashSet<IVec3>,
    pub bounds: HashMap<IVec2, (i32, i32)>,
    pub revision: u64,
}

impl TerrainStream {
    pub fn new(generator: Arc<TerrainGenerator>, seed: u64) -> io::Result<Self> {
        let (requests, work) = mpsc::sync_channel(MAX_PENDING);
        let (finished, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("terrain-generation".into())
            .spawn(move || {
                while let Ok(work) = work.recv() {
                    let result = match work {
                        Work::Survey(column) => Completed::Survey(
                            column,
                            generator.column_bounds(column.x, column.y, seed),
                        ),
                        Work::Generate(coord) => Completed::Generate(
                            coord,
                            Chunk::generate_with(coord, seed, &generator),
                        ),
                    };
                    if finished.send(result).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests,
            results: Mutex::new(results),
            pending_columns: HashSet::new(),
            pending_chunks: HashSet::new(),
            bounds: HashMap::new(),
            revision: 0,
        })
    }

    pub fn poll(&mut self) -> Vec<(IVec3, Chunk)> {
        let mut chunks = Vec::new();
        let receiver = self.results.get_mut();
        // At most MAX_PENDING results can exist, including stale requests.
        loop {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("terrain worker stopped unexpectedly")
                }
            };
            match result {
                Completed::Survey(column, bounds) => {
                    self.pending_columns.remove(&column);
                    self.bounds.insert(column, bounds);
                    self.revision += 1;
                }
                Completed::Generate(coord, chunk) => {
                    self.pending_chunks.remove(&coord);
                    chunks.push((coord, chunk));
                }
            }
        }
        chunks
    }

    pub fn generating(&self, coord: &IVec3) -> bool {
        self.pending_chunks.contains(coord)
    }

    fn capacity(&self) -> usize {
        MAX_PENDING - self.pending_chunks.len() - self.pending_columns.len()
    }

    pub fn generate(&mut self, coord: IVec3) {
        if self.capacity() > 0
            && self.pending_chunks.len() < MAX_PENDING_CHUNKS
            && !self.generating(&coord)
        {
            self.requests
                .try_send(Work::Generate(coord))
                .expect("terrain worker stopped");
            self.pending_chunks.insert(coord);
        }
    }

    pub fn survey(&mut self, centers: &[IVec3], radius: i32) {
        let mut wanted = HashSet::new();
        for center in centers {
            for z in -radius..=radius {
                for x in -radius..=radius {
                    let column = IVec2::new(center.x + x, center.z + z);
                    if column.x.abs_diff(0) <= 1_000_000 && column.y.abs_diff(0) <= 1_000_000 {
                        wanted.insert(column);
                    }
                }
            }
        }
        // Bound cache memory to current interest, including requests still in flight.
        self.bounds.retain(|column, _| wanted.contains(column));
        let missing = wanted
            .into_iter()
            .filter(|column| {
                !self.bounds.contains_key(column) && !self.pending_columns.contains(column)
            })
            .map(|column| IVec3::new(column.x, 0, column.y))
            .collect();
        for coord in nearest_chunks(missing, SURVEYS_PER_TICK.min(self.capacity()), |coord| {
            centers
                .iter()
                .map(|center| {
                    let delta = coord.as_i64vec3() - IVec3::new(center.x, 0, center.z).as_i64vec3();
                    delta.length_squared()
                })
                .min()
                .unwrap_or(0)
        }) {
            let column = IVec2::new(coord.x, coord.z);
            self.requests
                .try_send(Work::Survey(column))
                .expect("terrain worker stopped");
            self.pending_columns.insert(column);
        }
    }
}

/// Include surfaces, cliff walls, a local underground/flight volume, and edited chunks.
/// Survey neighbors are sampled too: a cliff may expose the side of a solid column.
pub(super) fn interests(
    position: Vec3,
    radius: i32,
    bounds: &HashMap<IVec2, (i32, i32)>,
    edited: impl Iterator<Item = IVec3>,
) -> HashSet<IVec3> {
    let center = chunk_coord(position.floor().as_ivec3());
    let mut result = local_chunks(position, 2.min(radius));
    for (&column, &(low, high)) in bounds {
        if column.x.abs_diff(center.x) > radius as u32
            || column.y.abs_diff(center.z) > radius as u32
        {
            continue;
        }
        let low = [IVec2::X, IVec2::NEG_X, IVec2::Y, IVec2::NEG_Y]
            .into_iter()
            .filter_map(|offset| bounds.get(&(column + offset)).map(|b| b.0))
            .fold(low, i32::min);
        let bottom = (low.div_euclid(CHUNK_SIZE) - 1).max(MIN_CHUNK_Y);
        let top = (high.div_euclid(CHUNK_SIZE) + 1).min(MAX_CHUNK_Y);
        for y in bottom..=top {
            result.insert(IVec3::new(column.x, y, column.y));
        }
    }
    for coord in edited {
        // Load a meshing halo around player changes, including towers above natural terrain.
        for neighbor in local_chunks((coord * CHUNK_SIZE).as_vec3(), 1) {
            if neighbor.x.abs_diff(center.x) <= radius as u32
                && neighbor.z.abs_diff(center.z) <= radius as u32
            {
                result.insert(neighbor);
            }
        }
    }
    result
}

pub(super) fn local_chunks(position: Vec3, radius: i32) -> HashSet<IVec3> {
    let center = chunk_coord(position.floor().as_ivec3());
    let mut result = HashSet::new();
    for y in -radius..=radius {
        for z in -radius..=radius {
            for x in -radius..=radius {
                let coord = center + IVec3::new(x, y, z);
                if valid_coord(coord) {
                    result.insert(coord);
                }
            }
        }
    }
    result
}

pub(super) fn spawn_position(generator: &TerrainGenerator, seed: u64) -> Vec3 {
    // Center of a single voxel leaves the full player footprint above its surface.
    // The smooth surface sits at raw_height; the topmost solid voxel is the
    // generation cap min(ceil(raw_height) - 1, height).
    let sample = generator.sample(0, 0, seed);
    let cap = (sample.raw_height.ceil() as i32 - 1).min(sample.height);
    Vec3::new(0.5, cap as f32 + 1.05, 0.5)
}

const SPAWN_SEARCH_HEIGHT: i32 = 64;

/// Small fallback volume to warm asynchronously if excavation removes every
/// supported landing in the initially loaded spawn chunks.
pub(super) fn spawn_search_chunks(preferred: Vec3) -> HashSet<IVec3> {
    let min = chunk_coord(
        (preferred - Vec3::new(9.0, (SPAWN_SEARCH_HEIGHT + 1) as f32, 9.0))
            .floor()
            .as_ivec3(),
    );
    let max = chunk_coord(
        (preferred + Vec3::new(9.0, (SPAWN_SEARCH_HEIGHT + 2) as f32, 9.0))
            .floor()
            .as_ivec3(),
    );
    let mut chunks = HashSet::new();
    for x in min.x..=max.x {
        for z in min.z..=max.z {
            for y in min.y..=max.y {
                let coord = IVec3::new(x, y, z);
                if valid_coord(coord) {
                    chunks.insert(coord);
                }
            }
        }
    }
    chunks
}

/// Search only the warm spawn volume for a supported, player-clear cell that also
/// avoids loose colliders. Joining and respawn never generate terrain on the tick;
/// when no safe cell is loaded the caller keeps the player dead for a later retry.
pub(super) fn available_spawn(
    world: &voxel_world::VoxelWorld,
    preferred: Vec3,
    bodies: &[physics::DynamicCollider],
) -> Option<Vec3> {
    for (dx, dz) in [
        (0, 0),
        (4, 0),
        (-4, 0),
        (0, 4),
        (0, -4),
        (8, 8),
        (-8, -8),
        (8, -8),
        (-8, 8),
    ] {
        for step in 0..SPAWN_SEARCH_HEIGHT * 2 {
            let dy = if step % 2 == 0 {
                step / 2
            } else {
                -(step + 1) / 2
            };
            let position = preferred + Vec3::new(dx as f32, dy as f32, dz as f32);
            let feet = position.floor().as_ivec3();
            if world
                .block(feet - IVec3::Y)
                .is_none_or(|b| b == voxel_world::AIR)
                || [feet, feet + IVec3::Y, feet + IVec3::Y * 2]
                    .into_iter()
                    .any(|p| world.block(p) != Some(voxel_world::AIR))
            {
                continue;
            }
            let min = position - Vec3::new(physics::PLAYER_RADIUS, 0.0, physics::PLAYER_RADIUS);
            let max = position
                + Vec3::new(
                    physics::PLAYER_RADIUS,
                    physics::PLAYER_HEIGHT,
                    physics::PLAYER_RADIUS,
                );
            if bodies.iter().all(|body| {
                !(min.cmplt(body.position + Vec3::splat(0.5)).all()
                    && max.cmpgt(body.position - Vec3::splat(0.5)).all())
            }) {
                return Some(position);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use voxel_world::AIR;

    #[test]
    fn interest_tracks_extreme_surfaces_cliffs_and_high_builds() {
        let bounds = HashMap::from([
            (IVec2::ZERO, (300, 350)),
            (IVec2::X, (-120, -80)),
            (IVec2::NEG_X, (20, 24)),
        ]);
        let chunks = interests(
            Vec3::new(0.5, 352.0, 0.5),
            16,
            &bounds,
            [IVec3::new(4, 20, 0)].into_iter(),
        );
        for y in -5..=11 {
            assert!(chunks.contains(&IVec3::new(0, y, 0)), "missing cliff {y}");
        }
        assert!(chunks.contains(&IVec3::new(1, -4, 0)));
        assert!(chunks.contains(&IVec3::new(4, 20, 0)));
        assert!(!chunks.contains(&IVec3::new(10, MIN_CHUNK_Y, 10)));
        assert!(chunks.iter().all(|c| valid_coord(*c)));
    }

    #[test]
    fn spawn_has_headroom_at_generated_height() {
        let generator = TerrainGenerator::default();
        for seed in [0, 7, 999] {
            let spawn = spawn_position(&generator, seed);
            let feet = spawn.floor().as_ivec3();
            for cell in [feet, feet + IVec3::Y] {
                let chunk = Chunk::generate_with(chunk_coord(cell), seed, &generator);
                assert_eq!(chunk.get(voxel_world::local_coord(cell)), AIR);
            }
            let ground = feet - IVec3::Y;
            let chunk = Chunk::generate_with(chunk_coord(ground), seed, &generator);
            assert_ne!(chunk.get(voxel_world::local_coord(ground)), AIR);
        }
    }

    #[test]
    fn worker_generation_is_bounded_and_matches_direct_generation() {
        let generator = Arc::new(TerrainGenerator::default());
        let mut stream = TerrainStream::new(generator.clone(), 7).unwrap();
        for x in 0..100 {
            stream.generate(IVec3::new(x, 0, 0));
        }
        assert_eq!(stream.pending_chunks.len(), MAX_PENDING_CHUNKS);
        stream.survey(&[IVec3::ZERO], 2);
        assert_eq!(
            stream.pending_columns.len(),
            MAX_PENDING - MAX_PENDING_CHUNKS
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut received = 0;
        while received < MAX_PENDING_CHUNKS || !stream.pending_columns.is_empty() {
            for (coord, chunk) in stream.poll() {
                assert_eq!(
                    chunk.runs(),
                    Chunk::generate_with(coord, 7, &generator).runs()
                );
                received += 1;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(stream.capacity(), MAX_PENDING);
    }

    fn spawn_world(seed: u64) -> (Vec3, voxel_world::VoxelWorld) {
        let generator = Arc::new(TerrainGenerator::default());
        let preferred = spawn_position(&generator, seed);
        let mut world = voxel_world::VoxelWorld {
            seed,
            generator,
            ..Default::default()
        };
        for coord in local_chunks(preferred, 2) {
            world.ensure_chunk(coord);
        }
        (preferred, world)
    }

    #[test]
    fn available_spawn_rejects_unloaded_terrain() {
        let world = voxel_world::VoxelWorld::default();
        assert!(available_spawn(&world, Vec3::new(0.5, 30.0, 0.5), &[]).is_none());
    }

    #[test]
    fn available_spawn_falls_back_from_dug_preferred_ground() {
        let (preferred, mut world) = spawn_world(3);
        let feet = preferred.floor().as_ivec3();
        world.set_block(feet - IVec3::Y, AIR).unwrap();
        let position = available_spawn(&world, preferred, &[]).expect("fallback spawn");
        assert_ne!(position, preferred);
        let feet = position.floor().as_ivec3();
        assert_ne!(world.block(feet - IVec3::Y), Some(AIR));
        for cell in [feet, feet + IVec3::Y, feet + IVec3::Y * 2] {
            assert_eq!(world.block(cell), Some(AIR), "blocked headroom at {cell}");
        }
        assert!((position - preferred).abs().max_element() <= 64.0);
    }

    #[test]
    fn available_spawn_rejects_fully_blocked_volume() {
        let (preferred, mut world) = spawn_world(3);
        let solid =
            Chunk::from_runs(0, &[(voxel_world::CHUNK_VOLUME as u16, voxel_world::STONE)]).unwrap();
        let center = chunk_coord(preferred.floor().as_ivec3());
        for dy in -3..=2 {
            for dz in -1..=1 {
                for dx in -1..=1 {
                    world.insert(center + IVec3::new(dx, dy, dz), solid.clone());
                }
            }
        }
        assert!(available_spawn(&world, preferred, &[]).is_none());
    }

    #[test]
    fn available_spawn_avoids_body_occupied_landing() {
        let (preferred, world) = spawn_world(3);
        assert_eq!(available_spawn(&world, preferred, &[]), Some(preferred));
        let body = physics::DynamicCollider::cube(7, preferred, Vec3::ZERO);
        let position = available_spawn(&world, preferred, &[body]).expect("clear fallback");
        assert_ne!(position, preferred);
        let min = position - Vec3::new(physics::PLAYER_RADIUS, 0.0, physics::PLAYER_RADIUS);
        let max = position
            + Vec3::new(
                physics::PLAYER_RADIUS,
                physics::PLAYER_HEIGHT,
                physics::PLAYER_RADIUS,
            );
        let overlaps = min.cmplt(body.position + Vec3::splat(0.5)).all()
            && max.cmpgt(body.position - Vec3::splat(0.5)).all();
        assert!(!overlaps, "spawn overlaps a loose body");
    }

    #[test]
    fn survey_prunes_cache_to_interest_and_reserves_chunk_capacity() {
        let generator = Arc::new(TerrainGenerator::default());
        let mut stream = TerrainStream::new(generator, 11).unwrap();
        for x in 0..64 {
            stream.generate(IVec3::new(x, 0, 0));
        }
        assert_eq!(stream.pending_chunks.len(), MAX_PENDING_CHUNKS);
        stream.bounds.insert(IVec2::new(9_000, 9_000), (10, 20));
        stream.bounds.insert(IVec2::new(1, 1), (10, 20));
        stream.survey(&[IVec3::ZERO], 2);
        assert!(!stream.bounds.contains_key(&IVec2::new(9_000, 9_000)));
        assert!(stream.bounds.contains_key(&IVec2::new(1, 1)));
        assert_eq!(
            stream.pending_columns.len(),
            MAX_PENDING - MAX_PENDING_CHUNKS
        );
    }

    #[test]
    fn bounded_surveys_eventually_cover_all_wanted_columns() {
        let generator = Arc::new(TerrainGenerator::default());
        let mut stream = TerrainStream::new(generator, 13).unwrap();
        let radius = 2;
        let wanted = ((2 * radius + 1) * (2 * radius + 1)) as usize;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            stream.survey(&[IVec3::ZERO], radius);
            stream.poll();
            if stream.bounds.len() == wanted {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "surveys did not converge"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(stream.pending_columns.is_empty());
    }
}
