use std::collections::HashMap;

use bevy::{
    asset::{RenderAssetUsages, embedded_asset},
    image::Image,
    camera::primitives::Aabb,
    light::{NotShadowCaster, NotShadowReceiver},
    mesh::Indices,
    pbr::{ExtendedMaterial, MaterialExtension},
    prelude::*,
    render::render_resource::{AsBindGroup, PrimitiveTopology},
    shader::ShaderRef,
    tasks::{AsyncComputeTaskPool, Task, block_on, futures_lite::future},
};
use voxel_world::{
    AIR, CHUNK_SIZE, ChunkNeighborhood, DIRT, GRASS, SAND, STONE, VoxelWorld,
    chunk_coord,
};

mod atlas;
use atlas::texture_atlas;

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
    /// Blades in the current grass field.
    pub grass_blades: usize,
}

#[derive(Default, Debug)]
pub struct MeshData {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    /// rgb = blended material tint; a = blend weight toward the second
    /// material id in `uvs.y` (0 for single-material vertices).
    pub colors: Vec<[f32; 4]>,
    /// x = primary material id, y = secondary material id for texture blending.
    pub uvs: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
}

impl MeshData {
    fn byte_len(&self) -> usize {
        self.positions.len() * 12
            + self.normals.len() * 12
            + self.colors.len() * 16
            + self.uvs.len() * 8
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
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, self.uvs)
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
        // Texture carries the material color; alpha carries the
        // secondary-material blend weight.
        self.colors.extend([[1.0, 1.0, 1.0, 0.0]; 4]);
        let m = material as f32;
        self.uvs.extend([[m, m]; 4]);
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
                // Vertex at the mean of the edge zero crossings; remember the
                // crossing nearest the vertex — its solid endpoint is the
                // surface voxel this vertex represents, which fixes material
                // bleed on slopes where a subsurface corner sits closer.
                let mut position = Vec3::ZERO;
                let mut crossings = 0.0;
                let mut nearest_crossing = f32::INFINITY;
                let mut surface_corner = 0usize;
                for &[a, b] in &EDGES {
                    let (da, db) = (d[a] as f32, d[b] as f32);
                    if (da > 0.0) == (db > 0.0) {
                        continue;
                    }
                    let t = da / (da - db);
                    let crossing = corner(c, a).as_vec3().lerp(corner(c, b).as_vec3(), t);
                    position += crossing;
                    crossings += 1.0;
                    let distance = (crossing - position / crossings).length_squared();
                    if distance < nearest_crossing {
                        nearest_crossing = distance;
                        surface_corner = if da > 0.0 { a } else { b };
                    }
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
                let material = neighborhood.block(corner(c, surface_corner));
                // Blend toward the second-nearest solid corner's material by
                // inverse distance so biome boundaries grade instead of
                // stepping. The surface corner dominates; a much closer second
                // corner only tints the transition band.
                let mut second: Option<(u8, f32)> = None;
                for i in 0..8 {
                    if d[i] <= 0 || i == surface_corner {
                        continue;
                    }
                    let distance = (position - corner(c, i).as_vec3()).length_squared();
                    if second.is_none_or(|(_, best)| distance < best) {
                        second = Some((i as u8, distance));
                    }
                }
                let (color, second_material) = match second {
                    Some((corner_index, second_distance)) => {
                        let other = neighborhood.block(corner(c, corner_index as usize));
                        let first_distance = (position
                            - corner(c, surface_corner).as_vec3())
                        .length_squared();
                        // Weight of the secondary material: 0.4 when the two
                        // corners are equidistant, fading to 0 as the second
                        // corner recedes past twice the surface distance.
                        let blend = (0.4
                            * (2.0 - second_distance / (first_distance + 1e-6)))
                        .clamp(0.0, 0.4);
                        // Texture carries the color; alpha carries the
                        // texture-space blend weight.
                        ([1.0, 1.0, 1.0, blend], other)
                    }
                    None => ([1.0, 1.0, 1.0, 0.0], material),
                };
                // Displace along the pre-displacement normal (kept as-is: the
                // bump is sub-voxel, so recomputing the gradient buys nothing).
                // The heightfield is a pure function of world position, so a
                // seam vertex shifts identically on both sides of a boundary.
                let world = (neighborhood.coord * CHUNK_SIZE).as_vec3() + position;
                position += normal * (noise2d(world.x, world.z) * roughness(material));
                vert_index[cell(c)] = mesh.positions.len() as u32;
                mesh.positions.push(position.to_array());
                mesh.normals.push(normal.to_array());
                mesh.colors.push(color);
                mesh.uvs.push([material as f32, second_material as f32]);
            }
        }
    }

    // Every sign-changing lattice edge starting in -1..32 spawns the quad dual
    // to it, through the four cells sharing the edge. An edge belongs to the
    // chunk containing its start lattice point; edges owned by a MISSING
    // neighbor are emitted here as a fallback so streaming gaps don't leave
    // cracks. Edges on +faces are owned by the +neighbor and skipped.
    for y in -1..CHUNK_SIZE {
        for z in -1..CHUNK_SIZE {
            for x in -1..CHUNK_SIZE {
                let c = IVec3::new(x, y, z);
                let owner = chunk_coord(c);
                if owner != IVec3::ZERO && neighborhood.has(owner) {
                    continue;
                }
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
                    let ring_cells = [c, c - eb, c - eb - ee, c - ee];
                    if ring_cells
                        .iter()
                        .any(|q| q.min_element() < -1 || q.max_element() >= CHUNK_SIZE)
                    {
                        continue;
                    }
                    let ring = ring_cells.map(|q| vert_index[cell(q)]);
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
    mesh.uvs.extend(placed.uvs);
    mesh.indices.extend(placed.indices.iter().map(|i| i + base));
    mesh
}

pub struct VoxelRenderPlugin;

impl Plugin for VoxelRenderPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "crates/voxel_render/src", "terrain.wgsl");
        embedded_asset!(app, "crates/voxel_render/src", "grass.wgsl");
        app.init_resource::<RenderFocus>()
            .init_resource::<VoxelRenderStats>()
            .init_resource::<Renderer>()
            .init_resource::<GrassField>()
            .add_plugins(MaterialPlugin::<TerrainMaterial>::default())
            .add_plugins(MaterialPlugin::<GrassMaterial> {
                prepass_enabled: false,
                shadows_enabled: false,
                ..default()
            })
            .add_systems(Startup, initialize_material)
            .add_systems(Update, (update_chunks, update_grass));
    }
}
/// Triplanar terrain material: the standard PBR pipeline plus a grayscale
/// noise atlas sampled by world position. Material ids ride in `uv0`; the
/// secondary-material blend weight rides in vertex alpha.
type TerrainMaterial = ExtendedMaterial<StandardMaterial, TerrainExtension>;

#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
struct TerrainExtension {
    #[texture(100, dimension = "2d_array")]
    #[sampler(101)]
    atlas: Handle<Image>,
}

impl MaterialExtension for TerrainExtension {
    fn fragment_shader() -> ShaderRef {
        "embedded://voxel_render/terrain.wgsl".into()
    }
}

#[derive(Resource, Default)]
struct Renderer {
    material: Handle<TerrainMaterial>,
    grass_material: Handle<GrassMaterial>,
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
    mut materials: ResMut<Assets<TerrainMaterial>>,
    mut grass_materials: ResMut<Assets<GrassMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    let atlas = images.add(texture_atlas());
    renderer.material = materials.add(ExtendedMaterial {
        base: StandardMaterial {
            base_color: Color::WHITE,
            perceptual_roughness: 0.95,
            ..default()
        },
        extension: TerrainExtension {
            atlas: atlas.clone(),
        },
    });
    renderer.grass_material = grass_materials.add(ExtendedMaterial {
        base: StandardMaterial {
            base_color: Color::WHITE,
            perceptual_roughness: 0.95,
            // Opaque two-sided ribbons: no alpha textures or transparency sorting.
            cull_mode: None,
            ..default()
        },
        extension: GrassExtension {
            atlas,
            lod_limits: Vec4::new(GRASS_NEAR_END, GRASS_MID_END, GRASS_RADIUS, GRASS as f32),
            density: Vec4::new(GRASS_LOD_DENSITY[0], GRASS_LOD_DENSITY[1], GRASS_LOD_DENSITY[2], 0.0),
        },
    });
}

// ---------------------------------------------------------------------------
// Grass
// ---------------------------------------------------------------------------

/// Full-detail blades per square metre near the camera. Farther patches keep
/// stable nested subsets selected by each blade's rank.
const GRASS_DENSITY: f32 = 10.0;
/// Grass patch edge length in metres. Patches snap to this world grid, so
/// blades never move when the camera does; patches only stream in and out.
const GRASS_PATCH_SIZE: i32 = 8;
/// Grass is kept within this radius of the focus (nearest patch edge).
const GRASS_RADIUS: f32 = 128.0;
/// Rank bands: blades with rank >= 0.3 fade out by 40 m, rank >= 0.12 by
/// 72 m, and the sparsest subset persists to `GRASS_RADIUS`. The shader
/// dissolves each blade into its root over its last quarter, so distance
/// reads as thinning rather than a hard edge or opaque dark fade ring.
const GRASS_NEAR_END: f32 = 40.0;
const GRASS_MID_END: f32 = 72.0;
const GRASS_LOD_DENSITY: [f32; 3] = [1.0, 0.3, 0.12];
/// Seconds between dependency-stamp scans; larger movement scans early.
const GRASS_STAMP_SCAN_INTERVAL: f32 = 0.12;
const GRASS_STAMP_SCAN_STEP: f32 = 4.0;
/// Patch builds/uploads per update, plus backlog relief so the ~800-patch
/// field fills in tens of frames after a teleport instead of many seconds.
const GRASS_BUILDS_PER_UPDATE: usize = 2;
const GRASS_BUILDS_PER_DIRTY: usize = 64;
/// Most distinct surface-bearing chunks meshed for one patch; bounds the cost
/// of a patch whose columns are shattered across many heights.
const GRASS_CHUNKS_PER_PATCH: usize = 8;
/// Blade height range, in metres.
const GRASS_HEIGHT: (f32, f32) = (0.32, 0.65);
/// Blade base width range, in metres. Broad blades suit the blocky world and
/// keep the field dense-looking at only ~10 blades/m2.
const GRASS_WIDTH: (f32, f32) = (0.12, 0.20);
/// Blend of the blade's geometric normal toward straight up. A blade is a
/// vertical ribbon, so its true normal is horizontal and an overhead sun
/// leaves it black; biasing upward is what makes grass read as lit.
const GRASS_NORMAL_UP: f32 = 0.6;
/// How far below the focus to keep scanning columns for terrain.
const GRASS_SCAN_DEPTH: i32 = 48;
/// Horizontal AABB padding, in metres: covers the shader's wind offset
/// (at most 0.225 * GRASS_HEIGHT.1) plus blades leaning past the patch edge.
const GRASS_BOUNDS_PAD: f32 = 0.4;

/// Cached five-vertex ribbons, animated and distance-scaled in `grass.wgsl`.
type GrassMaterial = ExtendedMaterial<StandardMaterial, GrassExtension>;

#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
struct GrassExtension {
    /// Shared with terrain: blade albedo is sampled at the root, not tinted.
    #[texture(101, dimension = "2d_array")]
    #[sampler(102)]
    atlas: Handle<Image>,
    #[uniform(100)]
    lod_limits: Vec4,
    #[uniform(103)]
    density: Vec4,
}

impl MaterialExtension for GrassExtension {
    fn vertex_shader() -> ShaderRef {
        "embedded://voxel_render/grass.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "embedded://voxel_render/grass.wgsl".into()
    }
}

/// Revisions of every chunk a patch build consulted, keyed by chunk coordinate.
/// `None` means the chunk was missing; its arrival (or removal, or any
/// revision bump inside) must rebuild the patch.
type PatchStamp = Vec<(IVec3, Option<u64>)>;

/// One `GRASS_PATCH_SIZE` square of the world grid.
struct GrassPatch {
    stamp: PatchStamp,
    draw: Option<(Entity, Handle<Mesh>)>,
    /// Blades baked into `draw`'s mesh (0 when no grass surface was found).
    blades: usize,
    ceiling: i32,
    lod: usize,
    /// Whether `stamp` reflects a real build; a fresh patch has an empty stamp
    /// but is dirty rather than "built over nothing".
    built: bool,
}

/// World-grid patches with throttled revision scans and budgeted rebuilds.
#[derive(Resource, Default)]
struct GrassField {
    patches: HashMap<IVec2, GrassPatch>,
    /// Reuse contour meshes across patches; never contour the same unchanged
    /// chunk once per patch. Entries are evicted with their dependent patches.
    surfaces: HashMap<IVec3, GrassSurface>,
    /// Scratch candidate list, `clear()`ed each frame; the allocation persists.
    dirty: Vec<(f32, IVec2)>,
    checked_at: f32,
    scan_focus: Vec3,
}

struct GrassSurface {
    stamp: Stamp,
    mesh: MeshData,
}

/// Bijective u64 mix (finaliser), the basis of all blade hashing.
fn mix64(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

/// Deterministic hash in `[0, 1)` from a seed and a salt.
fn unit_hash(seed: u64, salt: u64) -> f32 {
    (mix64(seed ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> 40) as f32
        / (1u64 << 24) as f32
}

/// Stable per-triangle seed: quantized vertex positions, so blades survive
/// chunk remeshes that reproduce bitwise-identical geometry.
fn triangle_seed(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> u64 {
    let mut seed = 0x243F_6A88_85A3_08D3u64;
    for v in [a, b, c] {
        for q in v {
            seed = mix64(seed ^ ((q * 256.0).round() as i64) as u64);
        }
    }
    seed
}

/// Record `coord` and its current revision (or absence) in the dependency
/// map, so the stamp detects any change in a chunk the build consulted.
fn probe(deps: &mut HashMap<IVec3, Option<u64>>, world: &VoxelWorld, coord: IVec3) {
    deps.entry(coord)
        .or_insert_with(|| world.chunks.get(&coord).map(|c| c.revision));
}

/// Topmost voxel with positive (solid) density in a world column, or `None`
/// when only air (loaded or missing) was found above the scan floor. The
/// smooth surface crosses the vertical edge between this voxel and the one
/// above it; every consulted chunk is recorded in `deps`.
fn column_top(
    world: &VoxelWorld,
    deps: &mut HashMap<IVec3, Option<u64>>,
    x: i32,
    z: i32,
    ceiling: i32,
) -> Option<i32> {
    for y in (ceiling - GRASS_SCAN_DEPTH..=ceiling).rev() {
        probe(deps, world, chunk_coord(IVec3::new(x, y, z)));
        // Missing chunks scan as air so unloaded sky cannot hide the terrain
        // below; the recorded dep rebuilds the patch when one streams in.
        let Some(density) = world.density(IVec3::new(x, y, z)) else {
            continue;
        };
        if density > 0 {
            return Some(y);
        }
    }
    None
}

/// Append one blade: a curved, tapered ribbon of five vertices and three
/// triangles (base pair, mid pair, tip). Positions are baked in world space
/// under an identity transform. Colour carries `(height_fraction, seed,
/// height, rank)` and UV carries the root's `(x, z)`, so the shader can
/// recover the root for wind phasing and LOD collapse.
fn push_blade(
    mesh: &mut MeshData,
    root: Vec3,
    height: f32,
    half_width: f32,
    yaw: f32,
    lean: f32,
    seed: f32,
    rank: f32,
) {
    let (sin, cos) = yaw.sin_cos();
    let facing = Vec3::new(cos, 0.0, sin);
    let right = Vec3::new(-sin, 0.0, cos);
    // Mid-vertex bend keeps the silhouette curved instead of a straight sliver.
    let mid = root + facing * (lean * 0.55 * 0.55 * height) + Vec3::Y * (0.55 * height);
    let tip = root + facing * (lean * height) + Vec3::Y * height;
    // The ribbon's geometric normal faces sideways; blending toward +Y is what
    // the sun actually lights.
    let face = right.cross(Vec3::Y - facing * lean).normalize_or_zero();
    let normal = (face * (1.0 - GRASS_NORMAL_UP) + Vec3::Y * GRASS_NORMAL_UP)
        .normalize_or_zero();
    let base = mesh.positions.len() as u32;
    mesh.positions.extend([
        (root - right * half_width).to_array(),
        (root + right * half_width).to_array(),
        (mid - right * half_width * 0.55).to_array(),
        (mid + right * half_width * 0.55).to_array(),
        tip.to_array(),
    ]);
    mesh.normals.extend([normal.to_array(); 5]);
    mesh.colors.extend([
        [0.0, seed, height, rank],
        [0.0, seed, height, rank],
        [0.55, seed, height, rank],
        [0.55, seed, height, rank],
        [1.0, seed, height, rank],
    ]);
    mesh.uvs.extend([[root.x, root.z]; 5]);
    mesh.indices.extend([base, base + 1, base + 2, base + 1, base + 3, base + 2]);
    mesh.indices.extend([base + 2, base + 3, base + 4]);
}

/// Everything needed to swap in a rebuilt patch.
struct GrassPatchData {
    mesh: MeshData,
    stamp: PatchStamp,
    bounds: Aabb,
    blades: usize,
}

/// Distance tier for a patch: which `GRASS_LOD_DENSITY` subset is emitted.
/// Determined by the patch's nearest edge to the focus; a patch rebuilds when
/// its tier changes so blades that can now appear are added.
fn grass_lod(coord: IVec2, focus: Vec2) -> usize {
    let size = GRASS_PATCH_SIZE as f32;
    let min = (coord * GRASS_PATCH_SIZE).as_vec2();
    let nearest = focus.clamp(min, min + Vec2::splat(size));
    let distance = nearest.distance(focus);
    if distance <= GRASS_NEAR_END {
        0
    } else if distance <= GRASS_MID_END {
        1
    } else {
        2
    }
}

/// Place roots barycentrically on the actual surface-net triangles. `lod`
/// bounds the rank subset emitted — blades ranked outside it have collapsed
/// before the patch could ever be drawn at this distance.
fn grass_patch_mesh(
    world: &VoxelWorld,
    patch: IVec2,
    ceiling: i32,
    lod: usize,
    surfaces: &mut HashMap<IVec3, GrassSurface>,
) -> GrassPatchData {
    let mut deps: HashMap<IVec3, Option<u64>> = HashMap::new();
    let min = patch * GRASS_PATCH_SIZE;
    let max = min + IVec2::splat(GRASS_PATCH_SIZE);

    // Column scan (one column past the patch edge on each side catches
    // boundary quads whose centroid lands inside): the top solid voxel fixes
    // which chunk holds the surface quads for that column.
    let mut wanted: HashMap<IVec3, u32> = HashMap::new();
    for z in min.y - 1..=max.y {
        for x in min.x - 1..=max.x {
            if let Some(top) = column_top(world, &mut deps, x, z, ceiling) {
                let coord = chunk_coord(IVec3::new(x, top, z));
                *wanted.entry(coord).or_default() += 1;
            }
        }
    }
    // Keep the chunks covering the most columns; deterministic even at cap.
    let mut wanted: Vec<(IVec3, u32)> = wanted.into_iter().collect();
    wanted.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| (a.0.x, a.0.y, a.0.z).cmp(&(b.0.x, b.0.y, b.0.z)))
    });
    wanted.truncate(GRASS_CHUNKS_PER_PATCH);

    let patch_min = min.as_vec2();
    let patch_max = max.as_vec2();
    let mut mesh = MeshData::default();
    for (coord, _) in wanted {
        if !world.chunks.contains_key(&coord) {
            continue;
        }
        // A triangle's corners are influenced by the whole 3x3x3 neighborhood,
        // so every contributing chunk revision joins the stamp.
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    probe(&mut deps, world, coord + IVec3::new(dx, dy, dz));
                }
            }
        }
        let stamp = world.mesh_stamp(coord);
        let surface = surfaces.entry(coord).or_insert_with(|| GrassSurface {
            stamp,
            mesh: surface_nets(&world.neighborhood(coord)),
        });
        if surface.stamp != stamp {
            surface.mesh = surface_nets(&world.neighborhood(coord));
            surface.stamp = stamp;
        }
        let terrain = &surface.mesh;
        let chunk_base = (coord * CHUNK_SIZE).as_vec3();
        for tri in terrain.indices.chunks_exact(3) {
            let (a, b, c) = (
                Vec3::from_array(terrain.positions[tri[0] as usize]) + chunk_base,
                Vec3::from_array(terrain.positions[tri[1] as usize]) + chunk_base,
                Vec3::from_array(terrain.positions[tri[2] as usize]) + chunk_base,
            );
            // Only upward GRASS surfaces grow blades; blending vertices keep
            // their own material id, so require all three primaries.
            if tri.iter().any(|&i| terrain.uvs[i as usize][0] != GRASS as f32) {
                continue;
            }
            let cross = (b - a).cross(c - a);
            let area = cross.length() * 0.5;
            if area < 1e-4 || cross.y / (area * 2.0) < 0.3 {
                continue;
            }
            // A quad ring can dip one cell outside its chunk; each triangle is
            // owned by exactly the patch containing its centroid, which keeps
            // blades from duplicating across patch borders.
            let centroid = (a + b + c) / 3.0;
            if centroid.x < patch_min.x
                || centroid.x >= patch_max.x
                || centroid.z < patch_min.y
                || centroid.z >= patch_max.y
            {
                continue;
            }
            let seed = triangle_seed(
                a.to_array(),
                b.to_array(),
                c.to_array(),
            );
            // Stochastic rounding preserves the expected area density.
            let count = (area * GRASS_DENSITY + unit_hash(seed, 0)) as u32;
            for i in 0..count {
                let r1 = unit_hash(seed, 2 * i as u64 + 1);
                let r2 = unit_hash(seed, 2 * i as u64 + 2);
                let su = r1.sqrt();
                let root = a * (1.0 - su) + b * (su * (1.0 - r2)) + c * (su * r2);
                // A placed cube sitting on the surface smothers blades under
                // it; check the cell above the root and one higher for tall
                // blades. Terrain can never occupy these cells on a topmost
                // surface, but placed cubes carry no density.
                let head = root.floor().as_ivec3();
                let top = (root + Vec3::Y * GRASS_HEIGHT.1).floor().as_ivec3();
                probe(&mut deps, world, chunk_coord(head));
                probe(&mut deps, world, chunk_coord(top));
                let smothered = [head, top].iter().any(|&cell| {
                    world
                        .voxel(cell)
                        .is_some_and(|voxel| voxel.placed && voxel.material != AIR)
                });
                if smothered {
                    continue;
                }
                let rank = unit_hash(seed, 0x6000 + i as u64);
                // This rank never appears inside the patch's distance tier;
                // skip it. A tier change rebuilds the patch and adds them.
                if rank >= GRASS_LOD_DENSITY[lod] {
                    continue;
                }
                let blade_seed = unit_hash(seed, 0x1000 + i as u64);
                let height = GRASS_HEIGHT.0
                    + (GRASS_HEIGHT.1 - GRASS_HEIGHT.0)
                        * unit_hash(seed, 0x2000 + i as u64);
                let half_width = (GRASS_WIDTH.0
                    + (GRASS_WIDTH.1 - GRASS_WIDTH.0) * unit_hash(seed, 0x3000 + i as u64))
                    * 0.5;
                let yaw = unit_hash(seed, 0x4000 + i as u64) * std::f32::consts::TAU;
                let lean = 0.15 + 0.35 * unit_hash(seed, 0x5000 + i as u64);
                push_blade(&mut mesh, root, height, half_width, yaw, lean, blade_seed, rank);
            }
        }
    }

    let blades = mesh.indices.len() / 9;
    let bounds = mesh.positions.iter().fold(None, |bounds: Option<(Vec3, Vec3)>, &p| {
        let p = Vec3::from_array(p);
        Some(match bounds {
            Some((lo, hi)) => (lo.min(p), hi.max(p)),
            None => (p, p),
        })
    });
    let bounds = bounds.map_or_else(
        || Aabb::from_min_max(Vec3::ZERO, Vec3::ZERO),
        |(lo, hi)| {
            let pad = Vec3::new(GRASS_BOUNDS_PAD, 0.0, GRASS_BOUNDS_PAD);
            Aabb::from_min_max(lo - pad, hi + pad)
        },
    );
    GrassPatchData {
        mesh,
        stamp: deps.into_iter().collect(),
        bounds,
        blades,
    }
}

/// Whether the world changed under a patch since its build: any consulted
/// chunk missing, appearing, or revised. Edits bump `Chunk::revision`, chunk
/// loads and unloads flip `Option`ness, so one comparison covers streaming
/// and player edits alike.
fn grass_stamp_dirty(world: &VoxelWorld, stamp: &PatchStamp) -> bool {
    stamp.iter().any(|&(coord, revision)| {
        world.chunks.get(&coord).map(|chunk| chunk.revision) != revision
    })
}

/// A patch square is in range while its nearest point is inside the radius.
fn grass_patch_in_range(coord: IVec2, focus: Vec2) -> bool {
    let size = GRASS_PATCH_SIZE as f32;
    let min = (coord * GRASS_PATCH_SIZE).as_vec2();
    let nearest = focus.clamp(min, min + Vec2::splat(size));
    nearest.distance_squared(focus) <= GRASS_RADIUS * GRASS_RADIUS
}

fn update_grass(
    mut commands: Commands,
    world: Res<VoxelWorld>,
    focus: Res<RenderFocus>,
    renderer: Res<Renderer>,
    mut field: ResMut<GrassField>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut stats: ResMut<VoxelRenderStats>,
    time: Res<Time>,
) {
    let focus_xz = Vec2::new(focus.0.x, focus.0.z);
    // A vertical band change must refresh even if XZ and revisions are stable.
    let ceiling = (focus.0.y.floor() as i32).div_euclid(8) * 8 + 16;

    // Drop out-of-range patches; their entities and mesh assets die with them.
    field.patches.retain(|&coord, patch| {
        if grass_patch_in_range(coord, focus_xz) {
            return true;
        }
        if let Some((entity, handle)) = patch.draw.take() {
            commands.entity(entity).despawn();
            meshes.remove(handle.id());
        }
        false
    });
    // Walking ~800 patch stamps per frame is too expensive; re-scan on a
    // short interval, and early after a 4 m move. Range/LOD/ceiling checks
    // stay per-frame, so updates never depend on stale timestamps.
    let scan = time.elapsed_secs() - field.checked_at >= GRASS_STAMP_SCAN_INTERVAL
        || focus.0.distance(field.scan_focus) >= GRASS_STAMP_SCAN_STEP;
    if scan {
        field.checked_at = time.elapsed_secs();
        field.scan_focus = focus.0;
    }
    let GrassField { patches, surfaces, .. } = &mut *field;
    surfaces.retain(|coord, _| {
        world.chunks.contains_key(coord)
            && patches.values().any(|patch| patch.stamp.iter().any(|(c, _)| c == coord))
    });

    // Gather dirty in-range patches, nearest first. `dirty` is reused scratch:
    // clear() keeps the allocation, so an unchanged world allocates nothing.
    field.dirty.clear();
    let size = GRASS_PATCH_SIZE as f32;
    let lo = ((focus_xz - Vec2::splat(GRASS_RADIUS)) / size)
        .floor()
        .as_ivec2()
        - IVec2::ONE;
    let hi = ((focus_xz + Vec2::splat(GRASS_RADIUS)) / size)
        .floor()
        .as_ivec2()
        + IVec2::ONE;
    for z in lo.y..=hi.y {
        for x in lo.x..=hi.x {
            let coord = IVec2::new(x, z);
            if !grass_patch_in_range(coord, focus_xz) {
                continue;
            }
            let lod = grass_lod(coord, focus_xz);
            let patch = field.patches.entry(coord).or_insert_with(|| GrassPatch {
                stamp: Vec::new(),
                draw: None,
                blades: 0,
                built: false,
                ceiling,
                lod,
            });
            if !patch.built
                || patch.ceiling != ceiling
                || patch.lod != lod
                || (scan && grass_stamp_dirty(&world, &patch.stamp))
            {
                let center = (coord.as_vec2() + 0.5) * size;
                field.dirty.push((center.distance_squared(focus_xz), coord));
            }
        }
    }
    field.dirty.sort_unstable_by(|a, b| {
        a.0.total_cmp(&b.0)
            .then_with(|| a.1.x.cmp(&b.1.x))
            .then_with(|| a.1.y.cmp(&b.1.y))
    });

    let builds = field.dirty.len().min(GRASS_BUILDS_PER_UPDATE + field.dirty.len() / GRASS_BUILDS_PER_DIRTY);
    for index in 0..builds {
        let coord = field.dirty[index].1;
        let lod = grass_lod(coord, focus_xz);
        let data = grass_patch_mesh(&world, coord, ceiling, lod, &mut field.surfaces);
        let patch = field.patches.get_mut(&coord).expect("in-range patch");
        patch.stamp = data.stamp;
        patch.blades = data.blades;
        patch.built = true;
        patch.ceiling = ceiling;
        patch.lod = lod;
        if let Some((entity, handle)) = patch.draw.take() {
            commands.entity(entity).despawn();
            meshes.remove(handle.id());
        }
        if !data.mesh.indices.is_empty() {
            let handle = meshes.add(data.mesh.into_mesh());
            let entity = commands
                .spawn((
                    Mesh3d(handle.clone()),
                    MeshMaterial3d(renderer.grass_material.clone()),
                    // Mesh positions are already world-space.
                    Transform::default(),
                    // Supplied bounds include the shader wind offset; the mesh
                    // is RENDER_WORLD so Bevy cannot compute one itself.
                    data.bounds,
                    NotShadowCaster,
                ))
                .id();
            patch.draw = Some((entity, handle));
        }
    }
    stats.grass_blades = field.patches.values().map(|patch| patch.blades).sum();
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
        CHUNK_VOLUME, Chunk, DENSITY_AIR, DIRT, GRASS, PLACED_FLAG, STONE, TerrainGenerator, WOOD,
        index,
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
            let material = mesh.uvs[start][0] as u8;
            for vertex in start..start + 4 {
                assert_eq!(mesh.normals[vertex], normal.to_array());
                // White tint; alpha carries the blend weight.
                assert_eq!(mesh.colors[vertex], [1.0, 1.0, 1.0, 0.0]);
                assert_eq!(mesh.uvs[vertex], [material as f32, material as f32]);
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
            assert_eq!(mesh.uvs[i][0], STONE as f32);
            // The interior top surface sits just below y = 16; the missing -y
            // neighbor also yields a fallback bottom surface near y = -0.5.
            if position[0] > 1.0 && position[0] < 31.0 && position[2] > 1.0 && position[2] < 31.0
            {
                assert!(
                    (position[1] > 15.0 && position[1] < 16.0)
                        || (position[1] > -1.0 && position[1] < 0.0),
                    "vertex at {position:?}"
                );
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
                // Surface material dominates; a small dirt blend is allowed.
                assert_eq!(mesh.uvs[i][0], GRASS as f32, "vertex at {position:?}");
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

    #[test]
    fn scatter_instances_sit_on_the_meshed_surface() {
        let generator = TerrainGenerator::default();
        let seed = 7;
        let coord = IVec3::new(0, 1, 0);
        let instances = generator.scatter_chunk(coord, seed);
        assert!(!instances.is_empty(), "no scatter to check");
        let mut world = VoxelWorld::default();
        for dz in -1..=1 {
            for dx in -1..=1 {
                let c = coord + IVec3::new(dx, 0, dz);
                world.insert(c, Chunk::generate_with(c, seed, &generator));
            }
        }
        let mesh = surface_nets(&world.neighborhood(coord));
        let base = (coord * CHUNK_SIZE).as_vec3();
        let mut highest = f32::MIN;
        let mut lowest = f32::MAX;
        for instance in &instances {
            let local = Vec3::new(instance.x, instance.y, instance.z) - base;
            // Nearest surface vertex in the instance's column. The y window
            // keeps cave ceilings far below from being mistaken for it.
            let nearest = mesh
                .positions
                .iter()
                .filter(|p| {
                    (p[0] - local.x).abs() < 1.0
                        && (p[2] - local.z).abs() < 1.0
                        && (p[1] - local.y).abs() < 4.0
                })
                .min_by(|a, b| (a[1] - local.y).abs().total_cmp(&(b[1] - local.y).abs()))
                .unwrap_or_else(|| panic!("no mesh vertex under {instance:?}"));
            let delta = local.y - nearest[1];
            highest = highest.max(delta);
            lowest = lowest.min(delta);
        }
        // Placing at the rounded block top rather than the contoured surface
        // floats every instance by 0.5..1.5 blocks.
        assert!(highest < 0.3, "scatter floats up to {highest} above the mesh");
        // Sink buries the base, but never by more than the authored amount.
        assert!(lowest > -1.0, "scatter buried {lowest} below the mesh");
    }

    fn renderer_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(bevy::asset::AssetPlugin::default())
            .init_resource::<VoxelWorld>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<TerrainMaterial>>()
            .init_resource::<Assets<GrassMaterial>>()
            .init_resource::<Assets<Image>>()
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

    /// Every root of every blade in the patch mesh.
    fn grass_roots(mesh: &MeshData) -> Vec<Vec3> {
        mesh.positions
            .chunks_exact(5)
            .map(|verts| {
                (Vec3::from_array(verts[0]) + Vec3::from_array(verts[1])) / 2.0
            })
            .collect()
    }

    /// Diagonal grass-over-dirt surface `x + y = 32` covering the test patches.
    fn sloped_grass_world() -> VoxelWorld {
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
        world
    }

    /// All upward GRASS triangles `surface_nets` emits for chunk (0,0,0), in
    /// world space: the authoritative surface blades must sit on.
    fn grass_surface_triangles(world: &VoxelWorld) -> Vec<(Vec3, Vec3, Vec3)> {
        let mesh = surface_nets(&world.neighborhood(IVec3::ZERO));
        let mut triangles = Vec::new();
        for tri in mesh.indices.chunks_exact(3) {
            if tri.iter().any(|&i| mesh.uvs[i as usize][0] != GRASS as f32) {
                continue;
            }
            let (a, b, c) = (
                Vec3::from_array(mesh.positions[tri[0] as usize]),
                Vec3::from_array(mesh.positions[tri[1] as usize]),
                Vec3::from_array(mesh.positions[tri[2] as usize]),
            );
            let cross = (b - a).cross(c - a);
            if cross.length() * 0.5 >= 1e-4 && cross.y / cross.length() >= 0.3 {
                triangles.push((a, b, c));
            }
        }
        triangles
    }

    /// Point `p` rests on triangle `(a, b, c)`: barycentric weights within the
    /// triangle and a near-zero distance to its plane.
    fn on_triangle(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> bool {
        let ab = b - a;
        let ac = c - a;
        let ap = p - a;
        let n = ab.cross(ac);
        if n.length_squared() < 1e-8 {
            return false;
        }
        let d00 = ab.dot(ab);
        let d01 = ab.dot(ac);
        let d11 = ac.dot(ac);
        let d20 = ap.dot(ab);
        let d21 = ap.dot(ac);
        let denom = d00 * d11 - d01 * d01;
        let v = (d11 * d20 - d01 * d21) / denom;
        let w = (d00 * d21 - d01 * d20) / denom;
        v >= -1e-3 && w >= -1e-3 && v + w <= 1.0 + 1e-3 && n.dot(ap).abs() <= 1e-3
    }

    #[test]
    fn grass_roots_lie_on_surface_nets_triangles() {
        // Roots must sit on the emitted sloped triangles, not integer columns.
        let world = sloped_grass_world();
        let data = grass_patch_mesh(&world, IVec2::ZERO, 48, 0, &mut HashMap::new());
        assert!(data.blades > 10, "patch grew only {} blades", data.blades);
        assert_eq!(data.mesh.positions.len(), data.blades * 5);
        assert_eq!(data.mesh.indices.len(), data.blades * 9);
        let triangles = grass_surface_triangles(&world);
        for blade in 0..data.blades {
            let verts = &data.mesh.positions[blade * 5..blade * 5 + 5];
            let color = data.mesh.colors[blade * 5];
            let uv = data.mesh.uvs[blade * 5];
            // Contract: root.xz rides in UV, height fraction and height in
            // color, so pos.y - color.x * color.z is the root's world Y.
            let root = Vec3::new(uv[0], verts[0][1] - color[0] * color[2], uv[1]);
            assert!(
                (verts[0][1] - root.y).abs() < 1e-5
                    && (verts[1][1] - root.y).abs() < 1e-5,
                "blade base not at root {root:?}"
            );
            assert!(
                (verts[4][1] - (root.y + color[2])).abs() < 1e-5,
                "tip not one blade-height above root"
            );
            assert!(
                triangles.iter().any(|&(a, b, c)| on_triangle(root, a, b, c)),
                "root {root:?} not on any emitted grass triangle"
            );
            // Base pair is genuinely wider than the near-pointed tip.
            let base_width =
                (Vec3::from_array(verts[1]) - Vec3::from_array(verts[0])).length();
            assert!(base_width >= GRASS_WIDTH.0 - 1e-4);
        }
    }

    #[test]
    fn grass_patch_rebuilds_only_when_consulted_chunks_change() {
        let mut world = sloped_grass_world();
        let mut surfaces = HashMap::new();
        let first = grass_patch_mesh(&world, IVec2::ZERO, 48, 0, &mut surfaces);
        let second = grass_patch_mesh(&world, IVec2::ZERO, 48, 0, &mut surfaces);
        assert_eq!(first.mesh.positions, second.mesh.positions);
        assert_eq!(first.mesh.indices, second.mesh.indices);
        assert!(!grass_stamp_dirty(&world, &first.stamp));

        // An edit inside the meshed chunk's neighborhood is noticed.
        world.set_block(IVec3::new(4, 5, 6), STONE);
        assert!(grass_stamp_dirty(&world, &first.stamp));
        world.set_block(IVec3::new(4, 5, 6), AIR);
        // Revision bumped again even though the content was restored.
        assert!(grass_stamp_dirty(&world, &first.stamp));

        // A far patch that never consulted the edited chunk stays clean;
        // loading a chunk it *did* scan (as missing) dirties it.
        let far = grass_patch_mesh(&world, IVec2::new(5, 5), 48, 0, &mut surfaces);
        assert!(!grass_stamp_dirty(&world, &far.stamp));
        world.insert(IVec3::new(1, 0, 1), chunk_with(|_| AIR));
        assert!(grass_stamp_dirty(&world, &far.stamp));
        let loaded = grass_patch_mesh(&world, IVec2::new(5, 5), 48, 0, &mut surfaces);

        // Unloading a consulted chunk is as dirty as editing it.
        world.remove(IVec3::new(1, 0, 1));
        assert!(grass_stamp_dirty(&world, &loaded.stamp));
    }

    #[test]
    fn placed_cube_smothers_blades_beneath_it() {
        let mut world = VoxelWorld::default();
        // Flat solid grass chunk: the top surface lands near y = 15.5 across
        // the whole patch, so blades root densely in every cell.
        world.insert(
            IVec3::ZERO,
            voxel_chunk_with(
                |p| if p.y < 16 { GRASS } else { AIR },
                |p| {
                    if p.y < 16 { 127 } else { DENSITY_AIR }
                },
            ),
        );
        let grown = grass_patch_mesh(&world, IVec2::ZERO, 48, 0, &mut HashMap::new());
        let cell_roots = |mesh: &MeshData| {
            grass_roots(mesh)
                .into_iter()
                .filter(|r| r.x >= 4.0 && r.x < 5.0 && r.z >= 4.0 && r.z < 5.0)
                .count()
        };
        assert!(
            cell_roots(&grown.mesh) > 0,
            "no blades rooted under the cube cell"
        );

        world.set_block(IVec3::new(4, 16, 4), STONE);
        let smothered = grass_patch_mesh(&world, IVec2::ZERO, 48, 0, &mut HashMap::new());
        assert_eq!(cell_roots(&smothered.mesh), 0);
        assert!(smothered.blades < grown.blades);
        assert!(smothered.blades > grown.blades - 20, "whole patch suppressed");
    }

    #[test]
    fn far_tier_keeps_nested_rank_subset() {
        // Same patch at full density vs the far tier: the far mesh is a strict
        // subset — rank is position-seeded, so blades return identically.
        let world = sloped_grass_world();
        let mut surfaces = HashMap::new();
        let near = grass_patch_mesh(&world, IVec2::ZERO, 48, 0, &mut surfaces);
        let far = grass_patch_mesh(&world, IVec2::ZERO, 48, 2, &mut surfaces);
        assert!(far.blades > 0);
        assert!(far.blades < near.blades * 3 / 10);
        for i in (0..far.mesh.colors.len()).step_by(5) {
            assert!(
                far.mesh.colors[i][3] < GRASS_LOD_DENSITY[2],
                "far-tier blade rank {} above threshold",
                far.mesh.colors[i][3]
            );
        }
        // Tiers partition distance monotonically away from the focus.
        let focus = Vec2::ZERO;
        assert_eq!(grass_lod(IVec2::ZERO, focus), 0);
        assert_eq!(grass_lod(IVec2::new(8, 0), focus), 1);
        assert_eq!(grass_lod(IVec2::new(15, 0), focus), 2);
        assert!(grass_patch_in_range(IVec2::new(15, 0), focus));
        assert!(!grass_patch_in_range(IVec2::new(17, 0), focus));
    }
}
