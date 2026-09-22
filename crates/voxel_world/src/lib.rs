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
/// Lowest world block coordinate covered by the chunk range.
pub const WORLD_MIN_Y: i32 = MIN_CHUNK_Y * CHUNK_SIZE;
/// Highest world block coordinate covered by the chunk range.
pub const WORLD_MAX_Y: i32 = (MAX_CHUNK_Y + 1) * CHUNK_SIZE - 1;
/// Density written for a fully solid terrain voxel.
pub const DENSITY_SOLID: i8 = 127;
/// Density written for a fully empty voxel.
pub const DENSITY_AIR: i8 = -128;
/// High bit packed into material bytes on the wire marking player-placed cubes.
pub const PLACED_FLAG: u8 = 0x80;
/// Largest brush radius accepted by [`VoxelWorld::brush_dig`] and
/// [`VoxelWorld::brush_add`]; bounds the touched-chunk work per edit.
pub const MAX_BRUSH_RADIUS: f32 = 8.0;

/// One voxel's full state: the material gameplay observes, the signed density
/// the smooth mesher contours, and whether the cell is a player-placed cube.
/// Invariant: `material != AIR` exactly when `density > 0 || placed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Voxel {
    pub material: u8,
    pub density: i8,
    pub placed: bool,
}
impl Voxel {
    pub const AIR: Self = Self {
        material: AIR,
        density: DENSITY_AIR,
        placed: false,
    };
    /// Terrain write: density implied by material, never placed.
    pub const fn terrain(material: u8) -> Self {
        Self {
            material,
            density: if material == AIR {
                DENSITY_AIR
            } else {
                DENSITY_SOLID
            },
            placed: false,
        }
    }
    /// Placed write: a discrete cube; density stays air so the smooth mesher
    /// ignores it and the greedy pass draws crisp faces.
    pub const fn placed(material: u8) -> Self {
        Self {
            material,
            density: DENSITY_AIR,
            placed: material != AIR,
        }
    }
    fn implied_density(material: u8) -> i8 {
        if material == AIR {
            DENSITY_AIR
        } else {
            DENSITY_SOLID
        }
    }
}

static DEFAULT_TERRAIN: LazyLock<TerrainGenerator> = LazyLock::new(TerrainGenerator::default);
/// All 27 chunks in the 3x3x3 neighborhood. Index 0 is the center chunk; the
/// remaining 26 are ordered by offset `(dx, dy, dz)` in `-1..=1` lexicographic
/// order skipping zero. Smooth meshing samples diagonal neighbors at chunk
/// corners, so the full neighborhood is required for seamless boundaries.
const OFFSETS: [IVec3; 27] = {
    let mut offsets = [IVec3::ZERO; 27];
    let mut i = 1;
    let mut dz = -1;
    while dz <= 1 {
        let mut dy = -1;
        while dy <= 1 {
            let mut dx = -1;
            while dx <= 1 {
                if dx != 0 || dy != 0 || dz != 0 {
                    offsets[i] = IVec3::new(dx, dy, dz);
                    i += 1;
                }
                dx += 1;
            }
            dy += 1;
        }
        dz += 1;
    }
    offsets
};

/// Material mid-tones (linear RGB from the sRGB palette). The texture atlas
/// carries the full shadow/mid/highlight ramp; this is the representative
/// color for UI, maps, and untextured consumers.
pub fn block_color(block: u8) -> [f32; 4] {
    match block {
        GRASS => [0.027, 0.072, 0.053, 1.0],  // #2D4A3E overgrown ivy
        DIRT => [0.051, 0.031, 0.022, 1.0],   // #3D3028 root umber
        STONE => [0.048, 0.060, 0.073, 1.0],  // #3C444B weathered flint
        SAND => [0.076, 0.049, 0.027, 1.0],   // #4A3C2C dark ochre
        WOOD => [0.024, 0.011, 0.006, 1.0],   // #2A1D14 dark timber
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
    /// Every voxel identical; material implies density, nothing placed.
    Uniform(u8),
    /// Packed material nibbles only; density implied, nothing placed.
    Dense(Box<[u8]>),
    /// Full voxel state: material nibbles, per-voxel density, placed bitmap.
    Smooth {
        materials: Box<[u8]>,
        density: Box<[i8]>,
        placed: Box<[u8]>,
    },
}
#[derive(Clone, Debug)]
pub struct Chunk {
    pub revision: u64,
    storage: Storage,
}
impl Chunk {
    fn material_at(&self, index: usize) -> u8 {
        match &self.storage {
            Storage::Uniform(block) => *block,
            Storage::Dense(bytes) | Storage::Smooth {
                materials: bytes, ..
            } => (bytes[index / 2] >> ((index & 1) * 4)) & 15,
        }
    }
    fn density_at(&self, index: usize) -> i8 {
        match &self.storage {
            Storage::Uniform(block) => Voxel::implied_density(*block),
            Storage::Dense(materials) => {
                let m = (materials[index / 2] >> ((index & 1) * 4)) & 15;
                Voxel::implied_density(m)
            }
            Storage::Smooth { density, .. } => density[index],
        }
    }
    fn placed_at(&self, index: usize) -> bool {
        match &self.storage {
            Storage::Uniform(_) | Storage::Dense(_) => false,
            Storage::Smooth { placed, .. } => placed[index / 8] & (1 << (index & 7)) != 0,
        }
    }
    /// Material gameplay observes; `AIR` when the voxel is non-solid.
    pub fn get(&self, local: IVec3) -> u8 {
        self.material_at(index(local))
    }
    /// Signed density at this voxel; `> 0` means the smooth surface is inside.
    pub fn density(&self, local: IVec3) -> i8 {
        self.density_at(index(local))
    }
    /// Whether this voxel is a player-placed discrete cube.
    pub fn placed(&self, local: IVec3) -> bool {
        self.placed_at(index(local))
    }
    /// Full voxel state for journaling and wire deltas.
    pub fn voxel(&self, local: IVec3) -> Voxel {
        let i = index(local);
        Voxel {
            material: self.material_at(i),
            density: self.density_at(i),
            placed: self.placed_at(i),
        }
    }
    fn set_material(&mut self, index: usize, block: u8) {
        match &mut self.storage {
            Storage::Dense(bytes) | Storage::Smooth {
                materials: bytes, ..
            } => {
                let shift = (index & 1) * 4;
                bytes[index / 2] = (bytes[index / 2] & !(15 << shift)) | (block << shift);
            }
            Storage::Uniform(_) => unreachable!("materials allocated before write"),
        }
    }
    /// Write the full voxel state. Expands storage tiers as needed.
    pub fn set_voxel(&mut self, index: usize, voxel: Voxel) {
        debug_assert!(voxel.material <= WOOD);
        let implied = voxel.density == Voxel::implied_density(voxel.material) && !voxel.placed;
        match &self.storage {
            Storage::Uniform(old) => {
                let old = *old;
                if implied && voxel.material == old {
                    return;
                }
                if implied {
                    self.storage =
                        Storage::Dense(vec![old | (old << 4); CHUNK_VOLUME / 2].into_boxed_slice());
                } else {
                    self.storage = Storage::Smooth {
                        materials: vec![old | (old << 4); CHUNK_VOLUME / 2].into_boxed_slice(),
                        density: vec![Voxel::implied_density(old); CHUNK_VOLUME].into_boxed_slice(),
                        placed: vec![0; CHUNK_VOLUME / 8].into_boxed_slice(),
                    };
                }
            }
            Storage::Dense(_) => {
                if !implied {
                    let Storage::Dense(materials) =
                        std::mem::replace(&mut self.storage, Storage::Uniform(AIR))
                    else {
                        unreachable!()
                    };
                    let mut density = Vec::with_capacity(CHUNK_VOLUME);
                    for i in 0..CHUNK_VOLUME {
                        let m = (materials[i / 2] >> ((i & 1) * 4)) & 15;
                        density.push(Voxel::implied_density(m));
                    }
                    self.storage = Storage::Smooth {
                        materials,
                        density: density.into_boxed_slice(),
                        placed: vec![0; CHUNK_VOLUME / 8].into_boxed_slice(),
                    };
                }
            }
            Storage::Smooth { .. } => {}
        }
        self.set_material(index, voxel.material);
        if let Storage::Smooth {
            density, placed, ..
        } = &mut self.storage
        {
            density[index] = voxel.density;
            if voxel.placed {
                placed[index / 8] |= 1 << (index & 7);
            } else {
                placed[index / 8] &= !(1 << (index & 7));
            }
        }
    }
    /// Legacy material-run decode used by tests and fixtures. Density is
    /// implied; nothing is placed.
    pub fn from_runs(revision: u64, runs: &[(u16, u8)]) -> Result<Self, String> {
        let mut total = 0usize;
        for &(count, block) in runs {
            if count == 0 || block > WOOD {
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
                chunk.set_material(i, block);
            }
            cursor += count as usize;
        }
        Ok(chunk)
    }
    /// Wire decode: material runs (high bit = placed) plus density runs.
    /// Either stream may be a single full-volume run.
    pub fn from_voxel_runs(
        revision: u64,
        material_runs: &[(u16, u8)],
        density_runs: &[(u16, i8)],
    ) -> Result<Self, String> {
        let expand = |runs: &[(u16, u8)]| -> Result<Vec<u8>, String> {
            let mut out = Vec::with_capacity(CHUNK_VOLUME);
            for &(count, block) in runs {
                if count == 0 || block & !PLACED_FLAG > WOOD {
                    return Err("zero run or invalid block".into());
                }
                if out.len() + count as usize > CHUNK_VOLUME {
                    return Err("chunk run volume overflow".into());
                }
                out.extend(std::iter::repeat(block).take(count as usize));
            }
            if out.len() != CHUNK_VOLUME {
                return Err("chunk run volume incomplete".into());
            }
            Ok(out)
        };
        let materials = expand(material_runs)?;
        let mut density = Vec::with_capacity(CHUNK_VOLUME);
        for &(count, d) in density_runs {
            if count == 0 {
                return Err("zero density run".into());
            }
            if density.len() + count as usize > CHUNK_VOLUME {
                return Err("density run volume overflow".into());
            }
            density.extend(std::iter::repeat(d).take(count as usize));
        }
        if density.len() != CHUNK_VOLUME {
            return Err("density run volume incomplete".into());
        }
        let mut chunk = Self {
            revision,
            storage: Storage::Uniform(AIR),
        };
        for i in 0..CHUNK_VOLUME {
            let raw = materials[i];
            chunk.set_voxel(
                i,
                Voxel {
                    material: raw & !PLACED_FLAG,
                    density: density[i],
                    placed: raw & PLACED_FLAG != 0,
                },
            );
        }
        chunk.compact();
        Ok(chunk)
    }
    /// Legacy material runs; placed voxels report their material with the
    /// placed flag set so wire consumers can reconstruct full state.
    pub fn runs(&self) -> Vec<(u16, u8)> {
        self.voxel_runs().0
    }
    /// Wire encode: material runs (high bit = placed) plus density runs.
    pub fn voxel_runs(&self) -> (Vec<(u16, u8)>, Vec<(u16, i8)>) {
        let mut material_runs: Vec<(u16, u8)> = Vec::new();
        let mut density_runs: Vec<(u16, i8)> = Vec::new();
        let mut material = self.material_at(0) | (self.placed_at(0) as u8) << 7;
        let mut density = self.density_at(0);
        let mut material_count = 1u16;
        let mut density_count = 1u16;
        for i in 1..CHUNK_VOLUME {
            let m = self.material_at(i) | (self.placed_at(i) as u8) << 7;
            let d = self.density_at(i);
            if m == material {
                material_count += 1;
            } else {
                material_runs.push((material_count, material));
                material = m;
                material_count = 1;
            }
            if d == density {
                density_count += 1;
            } else {
                density_runs.push((density_count, density));
                density = d;
                density_count = 1;
            }
        }
        material_runs.push((material_count, material));
        density_runs.push((density_count, density));
        (material_runs, density_runs)
    }
    pub fn is_empty(&self) -> bool {
        match &self.storage {
            Storage::Uniform(block) => *block == AIR,
            Storage::Dense(bytes) => bytes.iter().all(|&b| b == 0),
            Storage::Smooth {
                materials, placed, ..
            } => materials.iter().all(|&b| b == 0) && placed.iter().all(|&b| b == 0),
        }
    }
    pub fn generate(coord: IVec3, seed: u64) -> Self {
        Self::generate_with(coord, seed, &DEFAULT_TERRAIN)
    }

    /// Generate this chunk from a compiled terrain graph.
    pub fn generate_with(coord: IVec3, seed: u64, generator: &TerrainGenerator) -> Self {
        crate::terrain::generate_chunk(coord, seed, generator)
    }

    /// Collapse to the cheapest storage tier that preserves the voxel state.
    fn compact(&mut self) {
        if let Storage::Smooth {
            materials,
            density,
            placed,
        } = &self.storage
        {
            if placed.iter().all(|&b| b == 0)
                && (0..CHUNK_VOLUME).all(|i| {
                    let m = (materials[i / 2] >> ((i & 1) * 4)) & 15;
                    density[i] == Voxel::implied_density(m)
                })
            {
                let Storage::Smooth { materials, .. } =
                    std::mem::replace(&mut self.storage, Storage::Uniform(AIR))
                else {
                    unreachable!()
                };
                self.storage = Storage::Dense(materials);
            }
        }
        if matches!(self.storage, Storage::Dense(_)) {
            let first = self.material_at(0);
            if (1..CHUNK_VOLUME).all(|i| self.material_at(i) == first) {
                self.storage = Storage::Uniform(first);
            }
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
    /// Discrete-cube edit used by player placement and legacy callers.
    /// `block == AIR` clears the voxel entirely (material, density, placed);
    /// `block != AIR` writes a player-placed cube the smooth mesher ignores.
    /// Terrain carving goes through [`Self::brush_dig`].
    pub fn set_block(&mut self, pos: IVec3, block: u8) -> Option<(u64, u64)> {
        if block > WOOD {
            return None;
        }
        let voxel = if block == AIR {
            Voxel::AIR
        } else {
            Voxel::placed(block)
        };
        self.set_voxel(pos, voxel)
    }
    /// Write a full voxel state; used by journal restore and wire deltas.
    /// Returns `(old_revision, new_revision)` when the voxel changed.
    pub fn set_voxel(&mut self, pos: IVec3, voxel: Voxel) -> Option<(u64, u64)> {
        if voxel.material > WOOD {
            return None;
        }
        let chunk = self.chunks.get_mut(&chunk_coord(pos))?;
        let i = index(local_coord(pos));
        if chunk.voxel(local_coord(pos)) == voxel {
            return None;
        }
        let old = chunk.revision;
        let new = old.checked_add(1)?;
        let chunk = Arc::make_mut(chunk);
        chunk.set_voxel(i, voxel);
        chunk.revision = new;
        Some((old, new))
    }
    /// Signed density at a world voxel; `None` for unloaded chunks.
    pub fn density(&self, pos: IVec3) -> Option<i8> {
        self.chunks
            .get(&chunk_coord(pos))
            .map(|c| c.density(local_coord(pos)))
    }
    /// Full voxel state at a world position; `None` for unloaded chunks.
    pub fn voxel(&self, pos: IVec3) -> Option<Voxel> {
        self.chunks
            .get(&chunk_coord(pos))
            .map(|c| c.voxel(local_coord(pos)))
    }
    /// Trilinear density at a continuous world position. Samples the eight
    /// surrounding lattice points (the density stored at cell `c` is the
    /// lattice sample at `c`); `None` if any sample's chunk is unloaded.
    /// `> 0` is inside terrain. Used by smooth collision.
    pub fn density_at(&self, point: Vec3) -> Option<f32> {
        if !point.is_finite() {
            return None;
        }
        let base = point.floor().as_ivec3();
        let frac = point - base.as_vec3();
        let mut corners = [0.0f32; 8];
        for (i, corner) in corners.iter_mut().enumerate() {
            let offset = IVec3::new(
                (i & 1) as i32,
                ((i >> 1) & 1) as i32,
                ((i >> 2) & 1) as i32,
            );
            *corner = self.density(base + offset)? as f32;
        }
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let x00 = lerp(corners[0], corners[1], frac.x);
        let x10 = lerp(corners[2], corners[3], frac.x);
        let x01 = lerp(corners[4], corners[5], frac.x);
        let x11 = lerp(corners[6], corners[7], frac.x);
        Some(lerp(lerp(x00, x10, frac.y), lerp(x01, x11, frac.y), frac.z))
    }
    /// Density gradient at a continuous position via central differences on
    /// the trilinear field. `None` near unloaded chunks. Points toward
    /// increasing density (into terrain); negate for the surface normal.
    pub fn density_gradient(&self, point: Vec3) -> Option<Vec3> {
        const H: f32 = 0.25;
        let mut gradient = Vec3::ZERO;
        for axis in 0..3 {
            let mut offset = Vec3::ZERO;
            offset[axis] = H;
            let plus = self.density_at(point + offset)?;
            let minus = self.density_at(point - offset)?;
            gradient[axis] = (plus - minus) / (2.0 * H);
        }
        Some(gradient)
    }
    /// Carve a smooth sphere out of the terrain: subtracts density with a
    /// linear falloff and destroys placed cubes inside the radius. Returns the
    /// per-chunk `(old_revision, new_revision)` pairs and the voxels changed.
    pub fn brush_dig(&mut self, center: Vec3, radius: f32) -> Vec<BrushEdit> {
        self.brush(center, radius, None)
    }
    /// Deposit terrain: adds density with a linear falloff, adopting
    /// `material` where the voxel becomes solid. Placed cubes are untouched.
    pub fn brush_add(&mut self, center: Vec3, radius: f32, material: u8) -> Vec<BrushEdit> {
        if material == AIR || material > WOOD {
            return Vec::new();
        }
        self.brush(center, radius, Some(material))
    }
    fn brush(&mut self, center: Vec3, radius: f32, add: Option<u8>) -> Vec<BrushEdit> {
        if !center.is_finite()
            || !radius.is_finite()
            || radius <= 0.0
            || radius > MAX_BRUSH_RADIUS
        {
            return Vec::new();
        }
        let lo = (center - Vec3::splat(radius)).floor().as_ivec3();
        let hi = (center + Vec3::splat(radius)).floor().as_ivec3();
        let mut edits: Vec<BrushEdit> = Vec::new();
        for z in lo.z..=hi.z {
            for y in lo.y..=hi.y {
                for x in lo.x..=hi.x {
                    let pos = IVec3::new(x, y, z);
                    let distance = (pos.as_vec3() + Vec3::splat(0.5) - center).length();
                    if distance >= radius {
                        continue;
                    }
                    let coord = chunk_coord(pos);
                    let Some(chunk) = self.chunks.get_mut(&coord) else {
                        continue;
                    };
                    let local = local_coord(pos);
                    let before = chunk.voxel(local);
                    let delta = ((radius - distance) / radius * 255.0).min(255.0) as i32;
                    let after = match add {
                        Some(material) => {
                            if before.placed {
                                continue;
                            }
                            let density = (before.density as i32 + delta).min(127) as i8;
                            Voxel {
                                material: if density > 0 && before.material == AIR {
                                    material
                                } else {
                                    before.material
                                },
                                density,
                                placed: false,
                            }
                        }
                        None => {
                            let density = (before.density as i32 - delta).max(-128) as i8;
                            Voxel {
                                material: if density > 0 { before.material } else { AIR },
                                density,
                                placed: false,
                            }
                        }
                    };
                    if after == before {
                        continue;
                    }
                    let old = chunk.revision;
                    let Some(new) = old.checked_add(1) else {
                        continue;
                    };
                    let chunk = Arc::make_mut(chunk);
                    chunk.set_voxel(index(local), after);
                    chunk.revision = new;
                    match edits.iter_mut().find(|e: &&mut BrushEdit| e.coord == coord) {
                        Some(edit) => {
                            edit.to = new;
                            edit.voxels.push((index(local) as u16, before, after));
                        }
                        None => edits.push(BrushEdit {
                            coord,
                            from: old,
                            to: new,
                            voxels: vec![(index(local) as u16, before, after)],
                        }),
                    }
                }
            }
        }
        edits
    }
    pub fn mesh_stamp(&self, coord: IVec3) -> [Option<u64>; 27] {
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
    chunks: [Option<Arc<Chunk>>; 27],
}
impl ChunkNeighborhood {
    /// Map a local coordinate (possibly outside `0..32`) to its chunk slot and
    /// in-chunk coordinate. `local` must be within one chunk of the center.
    fn locate(&self, local: IVec3) -> (usize, IVec3) {
        let offset = chunk_coord(local);
        debug_assert!(
            offset.cmplt(IVec3::splat(2)).all() && offset.cmpgt(IVec3::splat(-2)).all(),
            "neighborhood sample beyond one chunk"
        );
        // OFFSETS[0] is the center; slots 1..=26 hold the other offsets in
        // (dz, dy, dx) lexicographic order. The zero triple's grid index is 13.
        let grid = ((offset.z + 1) * 9 + (offset.y + 1) * 3 + (offset.x + 1)) as usize;
        let slot = if grid == 13 {
            0
        } else if grid < 13 {
            grid + 1
        } else {
            grid
        };
        (slot, local_coord(local))
    }
    /// Whether the chunk at a neighborhood offset is loaded.
    pub fn has(&self, offset: IVec3) -> bool {
        let grid = ((offset.z + 1) * 9 + (offset.y + 1) * 3 + (offset.x + 1)) as usize;
        let slot = if grid == 13 {
            0
        } else if grid < 13 {
            grid + 1
        } else {
            grid
        };
        self.chunks[slot].is_some()
    }
    /// Material at a local coordinate; `AIR` for missing chunks.
    pub fn block(&self, local: IVec3) -> u8 {
        if local.cmpge(IVec3::ZERO).all() && local.cmplt(IVec3::splat(CHUNK_SIZE)).all() {
            return self.chunks[0]
                .as_ref()
                .map_or(AIR, |chunk| chunk.get(local));
        }
        let (slot, inner) = self.locate(local);
        self.chunks[slot].as_ref().map_or(AIR, |c| c.get(inner))
    }
    /// Signed density at a local coordinate; `DENSITY_AIR` for missing chunks.
    pub fn density(&self, local: IVec3) -> i8 {
        if local.cmpge(IVec3::ZERO).all() && local.cmplt(IVec3::splat(CHUNK_SIZE)).all() {
            return self.chunks[0]
                .as_ref()
                .map_or(DENSITY_AIR, |chunk| chunk.density(local));
        }
        let (slot, inner) = self.locate(local);
        self.chunks[slot]
            .as_ref()
            .map_or(DENSITY_AIR, |c| c.density(inner))
    }
    /// Whether a local coordinate is a player-placed cube.
    pub fn placed(&self, local: IVec3) -> bool {
        if local.cmpge(IVec3::ZERO).all() && local.cmplt(IVec3::splat(CHUNK_SIZE)).all() {
            return self.chunks[0]
                .as_ref()
                .is_some_and(|chunk| chunk.placed(local));
        }
        let (slot, inner) = self.locate(local);
        self.chunks[slot].as_ref().is_some_and(|c| c.placed(inner))
    }
    pub fn stamp(&self) -> [Option<u64>; 27] {
        std::array::from_fn(|i| self.chunks[i].as_ref().map(|c| c.revision))
    }
}
/// Per-chunk record of one brush application: revisions for staleness checks
/// and `(index, before, after)` triplets for journaling and wire deltas.
#[derive(Clone, Debug)]
pub struct BrushEdit {
    pub coord: IVec3,
    pub from: u64,
    pub to: u64,
    pub voxels: Vec<(u16, Voxel, Voxel)>,
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
        assert_eq!(world.set_block(p, 7), None);
        assert_eq!(world.mesh_stamp(IVec3::ZERO)[13], Some(1));
        world.remove(IVec3::NEG_X);
        assert_eq!(world.block(p), None);
        assert_eq!(world.mesh_stamp(IVec3::ZERO)[13], None);
    }
    #[test]
    fn strict_codec_and_nibble_boundaries() {
        for runs in [
            vec![],
            vec![(0, AIR), (32768, AIR)],
            vec![(32767, AIR)],
            vec![(32768, 7)],
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
