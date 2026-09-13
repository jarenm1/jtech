use bevy_app::{App, Plugin};
use bevy_ecs::prelude::Resource;
use glam::{IVec3, Vec3};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
};

pub mod terrain;
pub use terrain::{Biome, TerrainGenerator, TerrainSample};

pub const CHUNK_SIZE: i32 = 32;
pub const CHUNK_VOLUME: usize = 32768;
pub const MIN_CHUNK_Y: i32 = -8;
pub const MAX_CHUNK_Y: i32 = 23;
pub const AIR: u8 = 0;
pub const GRASS: u8 = 1;
pub const DIRT: u8 = 2;
pub const STONE: u8 = 3;
pub const SAND: u8 = 4;
pub const WOOD: u8 = 5;
/// Placeable respawn anchor: placing binds the owner's respawn, destroying
/// clears it. Soft like sand so raiding a bedroll is cheap.
pub const BEDROLL: u8 = 6;
/// Lowest world block coordinate covered by the chunk range.
pub const WORLD_MIN_Y: i32 = MIN_CHUNK_Y * CHUNK_SIZE;
/// Highest world block coordinate covered by the chunk range.
pub const WORLD_MAX_Y: i32 = (MAX_CHUNK_Y + 1) * CHUNK_SIZE - 1;

static DEFAULT_TERRAIN: LazyLock<TerrainGenerator> = LazyLock::new(TerrainGenerator::default);
const OFFSETS: [IVec3; 7] = [
    IVec3::ZERO,
    IVec3::X,
    IVec3::NEG_X,
    IVec3::Y,
    IVec3::NEG_Y,
    IVec3::Z,
    IVec3::NEG_Z,
];

pub fn block_color(block: u8) -> [f32; 4] {
    match block {
        GRASS => [0.14, 0.46, 0.07, 1.0],
        DIRT => [0.28, 0.13, 0.055, 1.0],
        STONE => [0.32, 0.35, 0.39, 1.0],
        SAND => [0.72, 0.57, 0.29, 1.0],
        WOOD => [0.24, 0.095, 0.035, 1.0],
        BEDROLL => [0.55, 0.42, 0.62, 1.0],
        _ => [0.0; 4],
    }
}

pub fn chunk_coord(pos: IVec3) -> IVec3 {
    IVec3::new(
        pos.x.div_euclid(32),
        pos.y.div_euclid(32),
        pos.z.div_euclid(32),
    )
}
pub fn local_coord(pos: IVec3) -> IVec3 {
    IVec3::new(
        pos.x.rem_euclid(32),
        pos.y.rem_euclid(32),
        pos.z.rem_euclid(32),
    )
}
pub fn index(local: IVec3) -> usize {
    assert!(
        local.cmpge(IVec3::ZERO).all() && local.cmplt(IVec3::splat(32)).all(),
        "local voxel outside chunk"
    );
    (local.x + 32 * (local.z + 32 * local.y)) as usize
}

#[derive(Clone, Debug)]
enum Storage {
    Uniform(u8),
    Dense(Box<[u8]>),
}
#[derive(Clone, Debug)]
pub struct Chunk {
    pub revision: u64,
    storage: Storage,
}
impl Chunk {
    fn at(&self, index: usize) -> u8 {
        match &self.storage {
            Storage::Uniform(block) => *block,
            Storage::Dense(bytes) => (bytes[index / 2] >> ((index & 1) * 4)) & 15,
        }
    }
    pub fn get(&self, local: IVec3) -> u8 {
        self.at(index(local))
    }
    fn set(&mut self, index: usize, block: u8) {
        if let Storage::Uniform(old) = self.storage {
            self.storage =
                Storage::Dense(vec![old | (old << 4); CHUNK_VOLUME / 2].into_boxed_slice());
        }
        if let Storage::Dense(bytes) = &mut self.storage {
            let shift = (index & 1) * 4;
            bytes[index / 2] = (bytes[index / 2] & !(15 << shift)) | (block << shift);
        }
    }
    pub fn from_runs(revision: u64, runs: &[(u16, u8)]) -> Result<Self, String> {
        let mut total = 0usize;
        for &(count, block) in runs {
            if count == 0 || block > BEDROLL {
                return Err("zero run or invalid block".into());
            }
            total += count as usize;
            if total > CHUNK_VOLUME {
                return Err("chunk run volume overflow".into());
            }
        }
        if total != CHUNK_VOLUME {
            return Err("chunk run volume incomplete".into());
        }
        let first = runs[0].1;
        if runs.iter().all(|&(_, block)| block == first) {
            return Ok(Self {
                revision,
                storage: Storage::Uniform(first),
            });
        }
        let mut chunk = Self {
            revision,
            storage: Storage::Dense(vec![0; CHUNK_VOLUME / 2].into_boxed_slice()),
        };
        let mut cursor = 0;
        for &(count, block) in runs {
            for i in cursor..cursor + count as usize {
                chunk.set(i, block);
            }
            cursor += count as usize;
        }
        Ok(chunk)
    }
    pub fn runs(&self) -> Vec<(u16, u8)> {
        if let Storage::Uniform(block) = self.storage {
            return vec![(CHUNK_VOLUME as u16, block)];
        }
        let mut runs = Vec::new();
        let mut block = self.at(0);
        let mut count = 1u16;
        for i in 1..CHUNK_VOLUME {
            let next = self.at(i);
            if next == block {
                count += 1;
            } else {
                runs.push((count, block));
                block = next;
                count = 1;
            }
        }
        runs.push((count, block));
        runs
    }
    pub fn is_empty(&self) -> bool {
        match &self.storage {
            Storage::Uniform(block) => *block == AIR,
            Storage::Dense(bytes) => bytes.iter().all(|&b| b == 0),
        }
    }
    pub fn generate(coord: IVec3, seed: u64) -> Self {
        Self::generate_with(coord, seed, &DEFAULT_TERRAIN)
    }

    /// Generate this chunk from a compiled terrain graph.
    pub fn generate_with(coord: IVec3, seed: u64, generator: &TerrainGenerator) -> Self {
        crate::terrain::generate_chunk(coord, seed, generator)
    }

    fn compact(&mut self) {
        let first = self.at(0);
        if (1..CHUNK_VOLUME).all(|i| self.at(i) == first) {
            self.storage = Storage::Uniform(first);
        }
    }
}

#[derive(Resource, Default)]
pub struct VoxelWorld {
    pub chunks: HashMap<IVec3, Arc<Chunk>>,
    pub seed: u64,
    /// Compiled terrain graph shared by streaming generation.
    pub generator: Arc<TerrainGenerator>,
}
pub struct WorldPlugin;
impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VoxelWorld>();
    }
}
impl VoxelWorld {
    pub fn block(&self, pos: IVec3) -> Option<u8> {
        self.chunks
            .get(&chunk_coord(pos))
            .map(|c| c.get(local_coord(pos)))
    }
    pub fn insert(&mut self, coord: IVec3, chunk: Chunk) {
        self.chunks.insert(coord, Arc::new(chunk));
    }
    pub fn remove(&mut self, coord: IVec3) {
        self.chunks.remove(&coord);
    }
    pub fn ensure_chunk(&mut self, coord: IVec3) {
        self.chunks
            .entry(coord)
            .or_insert_with(|| Arc::new(Chunk::generate_with(coord, self.seed, &self.generator)));
    }
    pub fn set_block(&mut self, pos: IVec3, block: u8) -> Option<(u64, u64)> {
        if block > BEDROLL {
            return None;
        }
        let chunk = self.chunks.get_mut(&chunk_coord(pos))?;
        let i = index(local_coord(pos));
        if chunk.at(i) == block {
            return None;
        }
        let old = chunk.revision;
        let new = old.checked_add(1)?;
        let chunk = Arc::make_mut(chunk);
        chunk.set(i, block);
        chunk.revision = new;
        Some((old, new))
    }
    pub fn mesh_stamp(&self, coord: IVec3) -> [Option<u64>; 7] {
        OFFSETS.map(|offset| self.chunks.get(&(coord + offset)).map(|c| c.revision))
    }
    pub fn neighborhood(&self, coord: IVec3) -> ChunkNeighborhood {
        ChunkNeighborhood {
            coord,
            chunks: OFFSETS.map(|offset| self.chunks.get(&(coord + offset)).cloned()),
        }
    }
    pub fn raycast(&self, origin: Vec3, direction: Vec3, max_distance: f32) -> Option<RayHit> {
        if !origin.is_finite()
            || !direction.is_finite()
            || !max_distance.is_finite()
            || max_distance < 0.0
            || origin.abs().max_element() > 32_000_000.0
        {
            return None;
        }
        let direction = direction.try_normalize()?;
        let mut cell = origin.floor().as_ivec3();
        let step = direction.signum().as_ivec3();
        let mut next = Vec3::splat(f32::INFINITY);
        let mut delta = Vec3::splat(f32::INFINITY);
        for axis in 0..3 {
            if direction[axis] != 0.0 {
                let boundary = cell[axis] as f32 + if direction[axis] > 0.0 { 1.0 } else { 0.0 };
                next[axis] = (boundary - origin[axis]) / direction[axis];
                delta[axis] = direction[axis].abs().recip();
            }
        }
        let mut adjacent = cell;
        let mut distance = 0.0;
        // Hard work bound even for an untrusted enormous distance through loaded air.
        for _ in 0..4096 {
            if self.block(cell)? != AIR {
                return Some(RayHit {
                    block: cell,
                    adjacent,
                    distance,
                });
            }
            let axis = if next.x <= next.y && next.x <= next.z {
                0
            } else if next.y <= next.z {
                1
            } else {
                2
            };
            distance = next[axis];
            if distance > max_distance {
                return None;
            }
            adjacent = cell;
            cell[axis] += step[axis];
            next[axis] += delta[axis];
        }
        None
    }
}
pub struct ChunkNeighborhood {
    pub coord: IVec3,
    chunks: [Option<Arc<Chunk>>; 7],
}
impl ChunkNeighborhood {
    pub fn block(&self, local: IVec3) -> u8 {
        // Almost every meshing sample is in the center chunk. Do not perform
        // Euclidean division and an axial-neighbor search for those samples.
        if local.cmpge(IVec3::ZERO).all() && local.cmplt(IVec3::splat(CHUNK_SIZE)).all() {
            return self.chunks[0]
                .as_ref()
                .map_or(AIR, |chunk| chunk.get(local));
        }
        let offset = chunk_coord(local);
        OFFSETS
            .iter()
            .position(|&o| o == offset)
            .and_then(|i| self.chunks[i].as_ref())
            .map_or(AIR, |c| c.get(local_coord(local)))
    }
    pub fn stamp(&self) -> [Option<u64>; 7] {
        std::array::from_fn(|i| self.chunks[i].as_ref().map(|c| c.revision))
    }
}
#[derive(Clone, Copy, Debug)]
pub struct RayHit {
    pub block: IVec3,
    pub adjacent: IVec3,
    pub distance: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn uniform(block: u8) -> Chunk {
        Chunk::from_runs(0, &[(32768, block)]).unwrap()
    }
    #[test]
    fn negative_seams_and_snapshot_revision_isolation() {
        let mut world = VoxelWorld::default();
        world.insert(IVec3::NEG_X, uniform(AIR));
        world.insert(IVec3::ZERO, uniform(AIR));
        let snapshot = world.neighborhood(IVec3::ZERO);
        let p = IVec3::new(-1, 0, 0);
        assert_eq!(local_coord(p), IVec3::new(31, 0, 0));
        assert_eq!(world.set_block(p, STONE), Some((0, 1)));
        assert_eq!(world.block(p), Some(STONE));
        assert_eq!(snapshot.block(p), AIR);
        assert_ne!(snapshot.stamp(), world.mesh_stamp(IVec3::ZERO));
        assert_eq!(world.set_block(p, STONE), None);
        assert_eq!(world.set_block(p, 6), None);
        assert_eq!(world.mesh_stamp(IVec3::ZERO)[2], Some(1));
        world.remove(IVec3::NEG_X);
        assert_eq!(world.block(p), None);
        assert_eq!(world.mesh_stamp(IVec3::ZERO)[2], None);
    }
    #[test]
    fn strict_codec_and_nibble_boundaries() {
        for runs in [
            vec![],
            vec![(0, AIR), (32768, AIR)],
            vec![(32767, AIR)],
            vec![(32768, 6)],
            vec![(32768, AIR), (1, AIR)],
        ] {
            assert!(Chunk::from_runs(0, &runs).is_err());
        }
        let runs = [(1, WOOD), (1, DIRT), (32765, STONE), (1, SAND)];
        let chunk = Chunk::from_runs(17, &runs).unwrap();
        assert_eq!(chunk.runs(), runs);
        assert_eq!(chunk.get(IVec3::ZERO), WOOD);
        assert_eq!(chunk.get(IVec3::X), DIRT);
        assert_eq!(chunk.get(IVec3::splat(31)), SAND);
        assert_eq!(chunk.revision, 17);
        assert!(uniform(AIR).is_empty());
    }
    #[test]
    fn ray_hits_negative_neighbor_and_stops_at_unloaded_space() {
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, uniform(AIR));
        world.insert(IVec3::NEG_X, uniform(AIR));
        world.set_block(IVec3::new(-2, 1, 1), STONE);
        let origin = Vec3::new(1.5, 1.5, 1.5);
        let hit = world.raycast(origin, Vec3::NEG_X, 4.0).unwrap();
        assert_eq!(hit.block, IVec3::new(-2, 1, 1));
        assert_eq!(hit.adjacent, IVec3::new(-1, 1, 1));
        assert_eq!(hit.distance, 2.5);
        assert!(world.raycast(origin, Vec3::NEG_X, 2.0).is_none());
        assert!(world.raycast(origin, Vec3::ZERO, 4.0).is_none());
        world.remove(IVec3::NEG_X);
        assert!(world.raycast(origin, Vec3::NEG_X, 4.0).is_none());
    }
    #[test]
    fn terrain_is_seeded_and_revision_exhaustion_is_atomic() {
        let coord = IVec3::new(-1, 1, 0);
        let first = Chunk::generate(coord, 7);
        assert_eq!(first.runs(), Chunk::generate(coord, 7).runs());
        assert_ne!(first.runs(), Chunk::generate(coord, 8).runs());
        assert_eq!(first.revision, 0);
        let mut world = VoxelWorld::default();
        world.insert(
            IVec3::ZERO,
            Chunk::from_runs(u64::MAX, &[(32768, AIR)]).unwrap(),
        );
        assert_eq!(world.set_block(IVec3::ZERO, STONE), None);
        assert_eq!(world.block(IVec3::ZERO), Some(AIR));
        assert_eq!(world.mesh_stamp(IVec3::ZERO)[0], Some(u64::MAX));
    }
}
