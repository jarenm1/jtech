use std::collections::HashMap;

use bevy::{
    asset::RenderAssetUsages,
    mesh::Indices,
    prelude::*,
    render::render_resource::PrimitiveTopology,
    tasks::{AsyncComputeTaskPool, Task, block_on, futures_lite::future},
};
use voxel_world::{
    AIR, CHUNK_SIZE, ChunkNeighborhood, DIRT, SAND, STONE, VoxelWorld, block_color,
};

const MAX_JOBS: usize = 16;
const STARTS_PER_FRAME: usize = 8;
const UPLOADS_PER_FRAME: usize = 8;
const UPLOAD_BYTES_PER_FRAME: usize = 8 * 1024 * 1024;
type Stamp = [Option<u64>; 27];

#[derive(Resource, Default)]
pub struct RenderFocus(pub Vec3);

#[derive(Resource, Default)]
pub struct VoxelRenderStats {
    /// Nonempty rendered chunks, not a count of chunks inside the camera frustum.
    pub visible_chunks: usize,
    pub pending_jobs: usize,
    pub triangles: usize,
    /// Cumulative vertex/index bytes uploaded (not driver allocation size).
    pub uploaded_bytes: u64,
    pub stale_jobs: u64,
}

#[derive(Default, Debug)]
pub struct MeshData {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub colors: Vec<[f32; 4]>,
    pub indices: Vec<u32>,
}

impl MeshData {
    fn byte_len(&self) -> usize {
        self.positions.len() * 12
            + self.normals.len() * 12
            + self.colors.len() * 16
            + self.indices.len() * 4
    }

    fn into_mesh(self) -> Mesh {
        // Drop the CPU asset copy after extraction; remeshing always uses packed world data.
        Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::RENDER_WORLD,
        )
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, self.positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, self.normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, self.colors)
        .with_inserted_indices(Indices::U32(self.indices))
    }

    fn quad(&mut self, origin: Vec3, u: Vec3, v: Vec3, normal: Vec3, material: u8) {
        let base = self.positions.len() as u32;
        self.positions.extend([
            origin.to_array(),
            (origin + u).to_array(),
            (origin + u + v).to_array(),
            (origin + v).to_array(),
        ]);
        self.normals.extend([normal.to_array(); 4]);
        self.colors.extend([block_color(material); 4]);
        if u.cross(v).dot(normal) > 0.0 {
            self.indices
                .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        } else {
            self.indices
                .extend([base, base + 2, base + 1, base, base + 3, base + 2]);
        }
    }
}

/// Greedily merges coplanar exposed faces of player-placed cubes. Terrain
/// density never culls a placed face: a cube buried in rock still draws.
/// Coordinates are chunk-local; neighbor chunks supply the occlusion samples.
/// Scratch space is one fixed 32x32 mask; output is bounded by six faces per voxel.
pub fn placed_mesh(neighborhood: &ChunkNeighborhood) -> MeshData {
    let mut mesh = MeshData::default();
    let size = CHUNK_SIZE as usize;
    let mut mask = [AIR; (CHUNK_SIZE * CHUNK_SIZE) as usize];
    for axis in 0..3 {
        let u_axis = (axis + 1) % 3;
        let v_axis = (axis + 2) % 3;
        for sign in [-1, 1] {
            let mut normal = IVec3::ZERO;
            normal[axis] = sign;
            for layer in 0..CHUNK_SIZE {
                for v in 0..size {
                    for u in 0..size {
                        let mut cell = IVec3::ZERO;
                        cell[axis] = layer;
                        cell[u_axis] = u as i32;
                        cell[v_axis] = v as i32;
                        mask[u + v * size] = if neighborhood.placed(cell)
                            && !neighborhood.placed(cell + normal)
                        {
                            neighborhood.block(cell)
                        } else {
                            AIR
                        };
                    }
                }
                for v in 0..size {
                    let mut u = 0;
                    while u < size {
                        let material = mask[u + v * size];
                        if material == AIR {
                            u += 1;
                            continue;
                        }
                        let mut width = 1;
                        while u + width < size && mask[u + width + v * size] == material {
                            width += 1;
                        }
                        let mut height = 1;
                        while v + height < size
                            && (u..u + width).all(|x| mask[x + (v + height) * size] == material)
                        {
                            height += 1;
                        }
                        let mut origin = Vec3::ZERO;
                        origin[axis] = (layer + i32::from(sign > 0)) as f32;
                        origin[u_axis] = u as f32;
                        origin[v_axis] = v as f32;
                        let mut du = Vec3::ZERO;
                        let mut dv = Vec3::ZERO;
                        du[u_axis] = width as f32;
                        dv[v_axis] = height as f32;
                        mesh.quad(origin, du, dv, normal.as_vec3(), material);
                        for row in v..v + height {
                            mask[row * size + u..row * size + u + width].fill(AIR);
                        }
                        u += width;
                    }
                }
            }
        }
    }
    mesh
}

/// Per-material surface roughness: the amplitude of the deterministic
/// heightfield displacing surface-net vertices along their normal. Grass,
/// wood, and bedroll stay flat; placed cubes never reach this path.
fn roughness(material: u8) -> f32 {
    match material {
        DIRT => 0.05,
        STONE => 0.15,
        SAND => 0.03,
        _ => 0.0,
    }
}

/// Seeded value noise in [-1, 1), a pure function of world position so both
/// sides of a chunk boundary displace a shared seam vertex identically.
fn noise2d(x: f32, z: f32) -> f32 {
    // Lattice hash: fixed seed, wrapping arithmetic, no table lookups.
    let hash = |ix: i32, iz: i32| -> f32 {
        let mut h = (ix as u32)
            .wrapping_mul(0x8da6b343)
            ^ (iz as u32).wrapping_mul(0xd8163841)
            ^ 0xcb1ab31f;
        h = h.wrapping_mul(h).wrapping_add(0x9e3779b9);
        h ^= h >> 16;
        (h & 0x00ff_ffff) as f32 / 0x0100_0000 as f32
    };
    let (ix, iz) = (x.floor() as i32, z.floor() as i32);
    let (fx, fz) = (x - ix as f32, z - iz as f32);
    // Smoothstep weights keep the field continuous across lattice lines.
    let (sx, sz) = (fx * fx * (3.0 - 2.0 * fx), fz * fz * (3.0 - 2.0 * fz));
    let (h00, h10) = (hash(ix, iz), hash(ix + 1, iz));
    let (h01, h11) = (hash(ix, iz + 1), hash(ix + 1, iz + 1));
    (h00 + (h10 - h00) * sx + ((h01 + (h11 - h01) * sx) - (h00 + (h10 - h00) * sx)) * sz) * 2.0
        - 1.0
}

/// Naive surface nets over the signed density field: one vertex per cell whose
/// eight corner densities straddle zero, quads spanning every sign-changing
/// lattice edge. Cells -1..=31 are evaluated so quads owned by this chunk can
/// reach one cell into negative neighbors; the 27-chunk neighborhood supplies
/// those samples. An edge belongs to the chunk containing its start lattice
/// point, so boundary quads are emitted exactly once and vertices coincide
/// bitwise across the seam (same world densities, same arithmetic).
pub fn surface_nets(neighborhood: &ChunkNeighborhood) -> MeshData {
    const LATTICE: i32 = CHUNK_SIZE + 2; // density samples at -1..=32
    const CELLS: i32 = CHUNK_SIZE + 1; // dual cells at -1..=31
    let lattice = |p: IVec3| -> usize {
        ((p.x + 1) + LATTICE * ((p.z + 1) + LATTICE * (p.y + 1))) as usize
    };
    let cell = |c: IVec3| -> usize {
        ((c.x + 1) + CELLS * ((c.z + 1) + CELLS * (c.y + 1))) as usize
    };
    let mut field = vec![0i8; (LATTICE * LATTICE * LATTICE) as usize];
    for y in -1..=CHUNK_SIZE {
        for z in -1..=CHUNK_SIZE {
            for x in -1..=CHUNK_SIZE {
                let p = IVec3::new(x, y, z);
                field[lattice(p)] = neighborhood.density(p);
            }
        }
    }

    // Corner (dx,dy,dz) is bit dx | dy<<1 | dz<<2; the 12 edges pair corners
    // differing in exactly one bit.
    const EDGES: [[usize; 2]; 12] = [
        [0, 1],
        [2, 3],
        [4, 5],
        [6, 7],
        [0, 2],
        [1, 3],
        [4, 6],
        [5, 7],
        [0, 4],
        [1, 5],
        [2, 6],
        [3, 7],
    ];
    let corner =
        |c: IVec3, i: usize| c + IVec3::new(i as i32 & 1, (i as i32 >> 1) & 1, (i >> 2) as i32);

    let mut mesh = MeshData::default();
    let mut vert_index = vec![u32::MAX; (CELLS * CELLS * CELLS) as usize];
    for y in -1..CHUNK_SIZE {
        for z in -1..CHUNK_SIZE {
            for x in -1..CHUNK_SIZE {
                let c = IVec3::new(x, y, z);
                let mut d = [0i8; 8];
                for (i, d) in d.iter_mut().enumerate() {
                    *d = field[lattice(corner(c, i))];
                }
                let solid = d[0] > 0;
                if d.iter().all(|&s| (s > 0) == solid) {
                    continue;
                }
                // Vertex at the mean of the edge zero crossings.
                let mut position = Vec3::ZERO;
                let mut crossings = 0.0;
                for &[a, b] in &EDGES {
                    let (da, db) = (d[a] as f32, d[b] as f32);
                    if (da > 0.0) == (db > 0.0) {
                        continue;
                    }
                    let t = da / (da - db);
                    position += corner(c, a).as_vec3().lerp(corner(c, b).as_vec3(), t);
                    crossings += 1.0;
                }
                position /= crossings;
                // Trilinear gradient at the vertex; the normal points at air.
                let t = position - c.as_vec3();
                let (tx, ty, tz) = (t.x, t.y, t.z);
                let wy = |i: usize| if i & 2 == 2 { ty } else { 1.0 - ty };
                let wz = |i: usize| if i & 4 == 4 { tz } else { 1.0 - tz };
                let wx = |i: usize| if i & 1 == 1 { tx } else { 1.0 - tx };
                let mut gradient = Vec3::ZERO;
                for i in 0..8 {
                    let s = d[i] as f32;
                    gradient.x += s * wy(i) * wz(i) * if i & 1 == 1 { 1.0 } else { -1.0 };
                    gradient.y += s * wx(i) * wz(i) * if i & 2 == 2 { 1.0 } else { -1.0 };
                    gradient.z += s * wx(i) * wy(i) * if i & 4 == 4 { 1.0 } else { -1.0 };
                }
                let normal = (-gradient).try_normalize().unwrap_or(Vec3::Y);
                // Color and roughness come from the solid corner nearest the
                // vertex: at a grass-over-dirt transition the vertex hugs the
                // grass corner, so subsurface dirt cannot bleed through.
                let nearest = (0..8)
                    .filter(|&i| d[i] > 0)
                    .min_by_key(|&i| {
                        (position - corner(c, i).as_vec3())
                            .length_squared()
                            .to_bits()
                    })
                    .unwrap();
                let material = neighborhood.block(corner(c, nearest));
                // Displace along the pre-displacement normal (kept as-is: the
                // bump is sub-voxel, so recomputing the gradient buys nothing).
                // The heightfield is a pure function of world position, so a
                // seam vertex shifts identically on both sides of a boundary.
                let world = (neighborhood.coord * CHUNK_SIZE).as_vec3() + position;
                position += normal * (noise2d(world.x, world.z) * roughness(material));
                vert_index[cell(c)] = mesh.positions.len() as u32;
                mesh.positions.push(position.to_array());
                mesh.normals.push(normal.to_array());
                mesh.colors.push(block_color(material));
            }
        }
    }

    // Every sign-changing lattice edge starting in 0..32 spawns the quad dual
    // to it, through the four cells sharing the edge. Edges on the +faces are
    // owned by the neighboring chunk and skipped here.
    for y in 0..CHUNK_SIZE {
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let c = IVec3::new(x, y, z);
                for axis in 0..3 {
                    let mut step = IVec3::ZERO;
                    step[axis] = 1;
                    let d0 = field[lattice(c)];
                    let d1 = field[lattice(c + step)];
                    if (d0 > 0) == (d1 > 0) {
                        continue;
                    }
                    let b = (axis + 1) % 3;
                    let e = (axis + 2) % 3;
                    let mut eb = IVec3::ZERO;
                    let mut ee = IVec3::ZERO;
                    eb[b] = 1;
                    ee[e] = 1;
                    let ring = [c, c - eb, c - eb - ee, c - ee].map(|q| vert_index[cell(q)]);
                    if ring.contains(&u32::MAX) {
                        continue;
                    }
                    // Air sits on the side the edge leaves solid toward.
                    let mut hint = Vec3::ZERO;
                    hint[axis] = if d0 > 0 { 1.0 } else { -1.0 };
                    let p = |i: usize| Vec3::from_array(mesh.positions[ring[i] as usize]);
                    let geometric = (p(2) - p(0)).cross(p(3) - p(1));
                    if geometric.dot(hint) > 0.0 {
                        mesh.indices
                            .extend([ring[0], ring[1], ring[2], ring[0], ring[2], ring[3]]);
                    } else {
                        mesh.indices
                            .extend([ring[0], ring[2], ring[1], ring[0], ring[3], ring[2]]);
                    }
                }
            }
        }
    }
    mesh
}

/// Terrain surface plus placed cubes; placed vertices are appended after the
/// smooth ones with indices rebased.
pub fn mesh_chunk(neighborhood: &ChunkNeighborhood) -> MeshData {
    let mut mesh = surface_nets(neighborhood);
    let placed = placed_mesh(neighborhood);
    let base = mesh.positions.len() as u32;
    mesh.positions.extend(placed.positions);
    mesh.normals.extend(placed.normals);
    mesh.colors.extend(placed.colors);
    mesh.indices.extend(placed.indices.iter().map(|i| i + base));
    mesh
}

pub struct VoxelRenderPlugin;

impl Plugin for VoxelRenderPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RenderFocus>()
            .init_resource::<VoxelRenderStats>()
            .init_resource::<Renderer>()
            .add_systems(Startup, initialize_material)
            .add_systems(Update, update_chunks);
    }
}

#[derive(Resource, Default)]
struct Renderer {
    material: Handle<StandardMaterial>,
    chunks: HashMap<IVec3, RenderedChunk>,
    jobs: Vec<MeshJob>,
}

struct RenderedChunk {
    stamp: Stamp,
    draw: Option<(Entity, Handle<Mesh>)>,
    triangles: usize,
}

struct MeshJob {
    coord: IVec3,
    stamp: Stamp,
    task: Task<MeshData>,
    ready: Option<MeshData>,
}

fn initialize_material(
    mut renderer: ResMut<Renderer>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    renderer.material = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        perceptual_roughness: 0.95,
        ..default()
    });
}

fn distance(coord: IVec3, focus: Vec3) -> f32 {
    (coord.as_vec3() * CHUNK_SIZE as f32 + Vec3::splat(CHUNK_SIZE as f32 * 0.5))
        .distance_squared(focus)
}

fn remove_draw(chunk: &mut RenderedChunk, commands: &mut Commands, meshes: &mut Assets<Mesh>) {
    if let Some((entity, handle)) = chunk.draw.take() {
        commands.entity(entity).despawn();
        meshes.remove(handle.id());
    }
}

fn update_chunks(
    mut commands: Commands,
    world: Res<VoxelWorld>,
    focus: Res<RenderFocus>,
    mut renderer: ResMut<Renderer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut stats: ResMut<VoxelRenderStats>,
) {
    renderer.chunks.retain(|coord, chunk| {
        if world.chunks.contains_key(coord) {
            return true;
        }
        remove_draw(chunk, &mut commands, &mut meshes);
        false
    });

    // Keep obsolete running tasks in their slots until they finish. Dropping a task that is
    // already computing synchronously cannot immediately stop it, so replacing it early
    // could violate the bound on actual concurrent work.
    for job in &mut renderer.jobs {
        if job.ready.is_none() {
            job.ready = block_on(future::poll_once(&mut job.task));
        }
    }
    renderer
        .jobs
        .sort_by(|a, b| distance(a.coord, focus.0).total_cmp(&distance(b.coord, focus.0)));
    let mut uploaded = 0;
    let mut bytes = 0;
    let mut i = 0;
    while i < renderer.jobs.len() {
        let job = &renderer.jobs[i];
        let Some(data) = job.ready.as_ref() else {
            i += 1;
            continue;
        };
        if !world.chunks.contains_key(&job.coord) || world.mesh_stamp(job.coord) != job.stamp {
            renderer.jobs.remove(i);
            stats.stale_jobs = stats.stale_jobs.saturating_add(1);
            continue;
        }
        let size = data.byte_len();
        // A pathological checkerboard chunk may exceed the byte budget: upload it alone
        // rather than starving it forever. Ready results still occupy bounded job slots.
        if uploaded >= UPLOADS_PER_FRAME || (uploaded > 0 && bytes + size > UPLOAD_BYTES_PER_FRAME)
        {
            i += 1;
            continue;
        }
        let mut job = renderer.jobs.remove(i);
        let data = job.ready.take().expect("ready result checked above");
        if let Some(mut old) = renderer.chunks.remove(&job.coord) {
            remove_draw(&mut old, &mut commands, &mut meshes);
        }
        let triangles = data.indices.len() / 3;
        let draw = if triangles == 0 {
            None
        } else {
            let handle = meshes.add(data.into_mesh());
            let entity = commands
                .spawn((
                    Mesh3d(handle.clone()),
                    MeshMaterial3d(renderer.material.clone()),
                    Transform::from_translation(job.coord.as_vec3() * CHUNK_SIZE as f32),
                ))
                .id();
            Some((entity, handle))
        };
        renderer.chunks.insert(
            job.coord,
            RenderedChunk {
                stamp: job.stamp,
                draw,
                triangles,
            },
        );
        uploaded += 1;
        bytes += size;
    }
    stats.uploaded_bytes = stats.uploaded_bytes.saturating_add(bytes as u64);

    // Bounded nearest-candidate selection, not a full allocation/sort of the world's chunks.
    let slots = STARTS_PER_FRAME.min(MAX_JOBS - renderer.jobs.len());
    let mut nearest = [(f32::INFINITY, IVec3::ZERO); STARTS_PER_FRAME];
    let mut nearest_len = 0;
    if slots > 0 {
        for &coord in world.chunks.keys() {
            if renderer.jobs.iter().any(|job| job.coord == coord) {
                continue;
            }
            let stamp = world.mesh_stamp(coord);
            if renderer
                .chunks
                .get(&coord)
                .is_some_and(|chunk| chunk.stamp == stamp)
            {
                continue;
            }
            let priority = distance(coord, focus.0);
            let at =
                nearest[..nearest_len].partition_point(|&(d, _)| d.total_cmp(&priority).is_le());
            if at < slots {
                let end = nearest_len.min(slots - 1);
                nearest.copy_within(at..end, at + 1);
                nearest[at] = (priority, coord);
                nearest_len = (nearest_len + 1).min(slots);
            }
        }
    }
    for &(_, coord) in &nearest[..nearest_len] {
        let neighborhood = world.neighborhood(coord);
        let stamp = neighborhood.stamp();
        // An all-air center still owns boundary quads against solid neighbors,
        // so meshing is skipped only when the whole neighborhood is empty.
        let empty = (-1..=1).all(|dz| {
            (-1..=1).all(|dy| {
                (-1..=1).all(|dx| {
                    world
                        .chunks
                        .get(&(coord + IVec3::new(dx, dy, dz)))
                        .map_or(true, |chunk| chunk.is_empty())
                })
            })
        });
        let task = AsyncComputeTaskPool::get().spawn(async move {
            if empty {
                MeshData::default()
            } else {
                mesh_chunk(&neighborhood)
            }
        });
        renderer.jobs.push(MeshJob {
            coord,
            stamp,
            task,
            ready: None,
        });
    }
    stats.visible_chunks = renderer
        .chunks
        .values()
        .filter(|chunk| chunk.draw.is_some())
        .count();
    stats.triangles = renderer.chunks.values().map(|chunk| chunk.triangles).sum();
    stats.pending_jobs = renderer.jobs.len();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use voxel_world::{
        CHUNK_VOLUME, Chunk, DENSITY_AIR, DIRT, GRASS, PLACED_FLAG, STONE, WOOD, index,
    };

    fn chunk_with(mut block: impl FnMut(IVec3) -> u8) -> Chunk {
        let mut cells = vec![AIR; CHUNK_VOLUME];
        for y in 0..CHUNK_SIZE {
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let local = IVec3::new(x, y, z);
                    cells[index(local)] = block(local);
                }
            }
        }
        let runs: Vec<_> = cells.into_iter().map(|b| (1, b)).collect();
        Chunk::from_runs(0, &runs).unwrap()
    }

    fn placed_chunk_with(mut block: impl FnMut(IVec3) -> u8) -> Chunk {
        let mut cells = vec![AIR; CHUNK_VOLUME];
        for y in 0..CHUNK_SIZE {
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let local = IVec3::new(x, y, z);
                    let material = block(local);
                    cells[index(local)] = if material == AIR {
                        AIR
                    } else {
                        material | PLACED_FLAG
                    };
                }
            }
        }
        let material_runs: Vec<_> = cells.into_iter().map(|b| (1, b)).collect();
        Chunk::from_voxel_runs(0, &material_runs, &[(CHUNK_VOLUME as u16, DENSITY_AIR)]).unwrap()
    }

    fn voxel_chunk_with(
        mut material: impl FnMut(IVec3) -> u8,
        mut density: impl FnMut(IVec3) -> i8,
    ) -> Chunk {
        let mut materials = Vec::with_capacity(CHUNK_VOLUME);
        let mut densities = Vec::with_capacity(CHUNK_VOLUME);
        for y in 0..CHUNK_SIZE {
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let local = IVec3::new(x, y, z);
                    materials.push((1, material(local)));
                    densities.push((1, density(local)));
                }
            }
        }
        Chunk::from_voxel_runs(0, &materials, &densities).unwrap()
    }

    // Expand each quad into its unit faces: detects gaps, duplicates, incorrect material
    // merges, bad boundary ownership and flipped geometry, not just a triangle count.
    fn assert_placed_surface(world: &VoxelWorld, coord: IVec3) -> MeshData {
        let neighborhood = world.neighborhood(coord);
        let mesh = placed_mesh(&neighborhood);
        let mut expected = HashSet::new();
        for y in 0..CHUNK_SIZE {
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let cell = IVec3::new(x, y, z);
                    if !neighborhood.placed(cell) {
                        continue;
                    }
                    let material = neighborhood.block(cell);
                    for axis in 0..3 {
                        for sign in [-1, 1] {
                            let mut normal = IVec3::ZERO;
                            normal[axis] = sign;
                            if !neighborhood.placed(cell + normal) {
                                expected.insert((cell.to_array(), axis, sign, material));
                            }
                        }
                    }
                }
            }
        }
        let mut actual = HashSet::new();
        for (quad, indices) in mesh.indices.chunks_exact(6).enumerate() {
            let start = quad * 4;
            let normal = Vec3::from_array(mesh.normals[start]);
            for triangle in indices.chunks_exact(3) {
                let a = Vec3::from_array(mesh.positions[triangle[0] as usize]);
                let b = Vec3::from_array(mesh.positions[triangle[1] as usize]);
                let c = Vec3::from_array(mesh.positions[triangle[2] as usize]);
                assert!((b - a).cross(c - a).dot(normal) > 0.0);
            }
            let axis = (0..3).find(|&axis| normal[axis] != 0.0).unwrap();
            let sign = normal[axis] as i32;
            let u = (axis + 1) % 3;
            let v = (axis + 2) % 3;
            let min = Vec3::from_array(mesh.positions[start]).as_ivec3();
            let max = Vec3::from_array(mesh.positions[start + 2]).as_ivec3();
            let material = (1..=6)
                .find(|&b| block_color(b) == mesh.colors[start])
                .unwrap();
            for vertex in start..start + 4 {
                assert_eq!(mesh.normals[vertex], normal.to_array());
                assert_eq!(mesh.colors[vertex], block_color(material));
            }
            for j in min[v]..max[v] {
                for i in min[u]..max[u] {
                    let mut cell = min;
                    cell[u] = i;
                    cell[v] = j;
                    cell[axis] -= i32::from(sign > 0);
                    assert!(
                        actual.insert((cell.to_array(), axis, sign, material)),
                        "duplicate unit face"
                    );
                }
            }
        }
        assert_eq!(actual, expected);
        mesh
    }

    #[test]
    fn placed_chunk_merges_to_six_outward_quads() {
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, placed_chunk_with(|_| STONE));
        assert_eq!(assert_placed_surface(&world, IVec3::ZERO).indices.len(), 36);
    }

    #[test]
    fn material_seams_do_not_merge_and_internal_solids_do_not_draw() {
        let mut world = VoxelWorld::default();
        world.insert(
            IVec3::ZERO,
            placed_chunk_with(|p| if p.x < 16 { STONE } else { DIRT }),
        );
        assert_eq!(assert_placed_surface(&world, IVec3::ZERO).indices.len(), 60);
    }

    #[test]
    fn axial_neighbors_occlude_and_removal_restores_boundary_faces() {
        let mut world = VoxelWorld::default();
        let coord = IVec3::new(-2, 0, -1);
        world.insert(coord, placed_chunk_with(|_| STONE));
        for offset in [
            IVec3::X,
            IVec3::NEG_X,
            IVec3::Y,
            IVec3::NEG_Y,
            IVec3::Z,
            IVec3::NEG_Z,
        ] {
            world.insert(coord + offset, placed_chunk_with(|_| DIRT));
        }
        assert!(assert_placed_surface(&world, coord).indices.is_empty());
        world.remove(coord + IVec3::NEG_X);
        assert_eq!(assert_placed_surface(&world, coord).indices.len(), 6);
    }

    #[test]
    fn irregular_cavity_and_chunk_edges_have_exact_face_coverage() {
        let mut world = VoxelWorld::default();
        world.insert(
            IVec3::ZERO,
            placed_chunk_with(|p| {
                if p.x < 5 && p.z < 6 && p.y < 4 && p != IVec3::new(2, 2, 2) {
                    if (p.x + p.y + p.z) % 3 == 0 {
                        DIRT
                    } else {
                        STONE
                    }
                } else {
                    AIR
                }
            }),
        );
        assert_placed_surface(&world, IVec3::ZERO);
    }

    #[test]
    fn fully_solid_neighborhood_has_no_terrain_surface() {
        let mut world = VoxelWorld::default();
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    world.insert(IVec3::new(dx, dy, dz), chunk_with(|_| STONE));
                }
            }
        }
        let mesh = mesh_chunk(&world.neighborhood(IVec3::ZERO));
        assert!(mesh.indices.is_empty());
    }

    #[test]
    fn half_solid_chunk_contours_an_upward_surface() {
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, chunk_with(|p| if p.y < 16 { STONE } else { AIR }));
        let mesh = mesh_chunk(&world.neighborhood(IVec3::ZERO));
        assert!(!mesh.indices.is_empty());
        // Only vertices referenced by quads matter; unreferenced halo cells
        // (e.g. the bottom surface owned by the missing -y chunk) are skipped.
        let mut referenced = vec![false; mesh.positions.len()];
        for &i in &mesh.indices {
            referenced[i as usize] = true;
        }
        for (i, &position) in mesh.positions.iter().enumerate() {
            if !referenced[i] {
                continue;
            }
            let normal = Vec3::from_array(mesh.normals[i]);
            assert!((normal.length() - 1.0).abs() < 1e-4);
            assert_eq!(mesh.colors[i], block_color(STONE));
            // The only interior surface is the top, just below y = 16.
            if position[0] > 1.0 && position[0] < 31.0 && position[2] > 1.0 && position[2] < 31.0
            {
                assert!(position[1] > 15.0 && position[1] < 16.0);
                assert_eq!(normal, Vec3::Y);
            }
        }
        // Every triangle's geometric winding agrees with its vertex normals.
        for triangle in mesh.indices.chunks_exact(3) {
            let a = Vec3::from_array(mesh.positions[triangle[0] as usize]);
            let b = Vec3::from_array(mesh.positions[triangle[1] as usize]);
            let c = Vec3::from_array(mesh.positions[triangle[2] as usize]);
            let normal_sum: Vec3 = triangle
                .iter()
                .map(|&i| Vec3::from_array(mesh.normals[i as usize]))
                .sum();
            assert!((b - a).cross(c - a).dot(normal_sum) > 0.0);
        }
    }

    #[test]
    fn placed_cube_produces_six_faces_and_no_terrain() {
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, chunk_with(|_| AIR));
        world.set_block(IVec3::new(4, 5, 6), WOOD);
        let neighborhood = world.neighborhood(IVec3::ZERO);
        assert!(surface_nets(&neighborhood).indices.is_empty());
        let mesh = mesh_chunk(&neighborhood);
        assert_eq!(mesh.indices.len(), 36);
        assert_eq!(mesh.positions.len(), 24);
    }

    #[test]
    fn chunk_boundary_shares_vertices_without_duplicate_quads() {
        let mut world = VoxelWorld::default();
        let half = || chunk_with(|p| if p.y < 16 { STONE } else { AIR });
        world.insert(IVec3::ZERO, half());
        world.insert(IVec3::X, half());
        let a = mesh_chunk(&world.neighborhood(IVec3::ZERO));
        let b = mesh_chunk(&world.neighborhood(IVec3::X));
        // Vertices on the shared seam are bitwise identical: chunk B's halo
        // cell -1 is chunk A's cell 31, computed from the same densities.
        let seam = |mesh: &MeshData, offset: f32, near: fn(f32) -> bool| {
            mesh.positions
                .iter()
                .filter(|p| near(p[0]))
                .map(|p| {
                    let mut q = *p;
                    q[0] += offset;
                    q.map(f32::to_bits)
                })
                .collect::<HashSet<_>>()
        };
        assert_eq!(
            seam(&a, 0.0, |x| x > 31.0),
            seam(&b, 32.0, |x| x <= 0.0)
        );
        // No triangle is emitted by both chunks.
        let triangles = |mesh: &MeshData, offset: f32| {
            mesh.indices
                .chunks_exact(3)
                .map(|t| {
                    let mut corners: Vec<_> = t
                        .iter()
                        .map(|&i| {

                            let mut p = mesh.positions[i as usize];
                            p[0] += offset;
                            p.map(f32::to_bits)
                        })
                        .collect();
                    corners.sort();
                    corners
                })
                .collect::<Vec<_>>()
        };
        let mut seen = HashSet::new();
        for triangle in triangles(&a, 0.0).into_iter().chain(triangles(&b, 32.0)) {
            assert!(seen.insert(triangle), "duplicate quad across chunk seam");
        }
    }

    #[test]
    fn grass_over_dirt_vertex_colors_follow_nearest_solid_corner() {
        // Diagonal surface x+y=32 through a grass cap over denser dirt: the
        // densest-corner pick would bleed dirt, the nearest corner is grass.
        let mut world = VoxelWorld::default();
        world.insert(
            IVec3::ZERO,
            voxel_chunk_with(
                |p| {
                    if p.x + p.y >= 32 {
                        AIR
                    } else if p.x + p.y >= 31 {
                        GRASS
                    } else {
                        DIRT
                    }
                },
                |p| {
                    if p.x + p.y >= 32 {
                        DENSITY_AIR
                    } else if p.x + p.y >= 31 {
                        40
                    } else {
                        127
                    }
                },
            ),
        );
        let mesh = surface_nets(&world.neighborhood(IVec3::ZERO));
        let mut matched = 0;
        for (i, &position) in mesh.positions.iter().enumerate() {
            if position[0] + position[1] > 30.0
                && position.iter().all(|&c| c > 1.0 && c < 31.0)
            {
                assert_eq!(mesh.colors[i], block_color(GRASS), "vertex at {position:?}");
                matched += 1;
            }
        }
        assert!(matched > 30);
    }

    #[test]
    fn roughness_displaces_stone_but_not_grass() {
        // Flat surface at y=16: undisplaced vertices sit at 15 + 127/255.
        let base = 15.0 + 127.0 / 255.0;
        let deviation = |material: u8| {
            let mut world = VoxelWorld::default();
            world.insert(
                IVec3::ZERO,
                chunk_with(|p| if p.y < 16 { material } else { AIR }),
            );
            let mesh = surface_nets(&world.neighborhood(IVec3::ZERO));
            mesh.positions
                .iter()
                .filter(|p| {
                    p[0] > 1.0 && p[0] < 31.0 && p[2] > 1.0 && p[2] < 31.0 && p[1] > 14.0
                })
                .map(|p| (p[1] - base).abs())
                .fold(0.0, f32::max)
        };
        let grass = deviation(GRASS);
        let stone = deviation(STONE);
        assert_eq!(grass, 0.0);
        assert!(stone > grass && stone <= 0.15);
    }

    #[test]
    fn seam_vertices_displace_identically_across_chunks() {
        let mut world = VoxelWorld::default();
        let half = || chunk_with(|p| if p.y < 16 { STONE } else { AIR });
        world.insert(IVec3::ZERO, half());
        world.insert(IVec3::X, half());
        let a = surface_nets(&world.neighborhood(IVec3::ZERO));
        let b = surface_nets(&world.neighborhood(IVec3::X));
        // Same world densities and the same world-space heightfield: shared
        // seam vertices must match bitwise after the +32 x-offset.
        let seam = |mesh: &MeshData, offset: f32, near: fn(f32) -> bool| {
            mesh.positions
                .iter()
                .filter(|p| near(p[0]))
                .map(|p| {
                    let mut q = *p;
                    q[0] += offset;
                    q.map(f32::to_bits)
                })
                .collect::<HashSet<_>>()
        };
        let a_seam = seam(&a, 0.0, |x| x > 31.0);
        assert_eq!(a_seam, seam(&b, 32.0, |x| x <= 0.0));
        // Stone roughness must actually move the seam off the flat plane.
        let base = 15.0 + 127.0 / 255.0;
        assert!(
            a_seam
                .iter()
                .any(|p| (f32::from_bits(p[1]) - base).abs() > 1e-6)
        );
    }

    fn renderer_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<VoxelWorld>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .add_plugins(VoxelRenderPlugin);
        app.update();
        app
    }

    fn ready_job(world: &VoxelWorld, coord: IVec3) -> MeshJob {
        let neighborhood = world.neighborhood(coord);
        MeshJob {
            coord,
            stamp: neighborhood.stamp(),
            task: AsyncComputeTaskPool::get().spawn(async { MeshData::default() }),
            ready: Some(mesh_chunk(&neighborhood)),
        }
    }

    #[test]
    fn neighbor_change_rejects_completed_mesh_before_upload() {
        let mut app = renderer_app();
        app.world_mut()
            .resource_mut::<VoxelWorld>()
            .insert(IVec3::ZERO, placed_chunk_with(|_| STONE));
        let job = ready_job(app.world().resource::<VoxelWorld>(), IVec3::ZERO);
        app.world_mut().resource_mut::<Renderer>().jobs.push(job);
        app.world_mut()
            .resource_mut::<VoxelWorld>()
            .insert(IVec3::X, placed_chunk_with(|_| STONE));
        app.update();
        let stats = app.world().resource::<VoxelRenderStats>();
        assert_eq!(stats.stale_jobs, 1);
        assert_eq!(stats.visible_chunks, 0);
        assert_eq!(stats.uploaded_bytes, 0);
        // Both dirty chunks must be requeued, including the rejected center.
        let renderer = app.world().resource::<Renderer>();
        assert!(renderer.jobs.iter().any(|job| job.coord == IVec3::ZERO));
        assert!(renderer.jobs.iter().any(|job| job.coord == IVec3::X));
    }

    #[test]
    fn unloading_releases_render_entity_and_mesh_asset() {
        let mut app = renderer_app();
        app.world_mut()
            .resource_mut::<VoxelWorld>()
            .insert(IVec3::ZERO, placed_chunk_with(|_| STONE));
        let job = ready_job(app.world().resource::<VoxelWorld>(), IVec3::ZERO);
        app.world_mut().resource_mut::<Renderer>().jobs.push(job);
        app.update();
        let (entity, mesh) = app.world().resource::<Renderer>().chunks[&IVec3::ZERO]
            .draw
            .clone()
            .unwrap();
        assert!(app.world().resource::<Assets<Mesh>>().contains(mesh.id()));
        assert_eq!(app.world().resource::<VoxelRenderStats>().triangles, 12);
        app.world_mut()
            .resource_mut::<VoxelWorld>()
            .remove(IVec3::ZERO);
        app.update();
        assert!(app.world().get_entity(entity).is_err());
        assert!(!app.world().resource::<Assets<Mesh>>().contains(mesh.id()));
        assert_eq!(app.world().resource::<VoxelRenderStats>().visible_chunks, 0);
        assert_eq!(app.world().resource::<VoxelRenderStats>().triangles, 0);
    }
}
