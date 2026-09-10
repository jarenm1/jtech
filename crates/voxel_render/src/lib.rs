use std::collections::HashMap;

use bevy::{
    asset::RenderAssetUsages,
    mesh::Indices,
    prelude::*,
    render::render_resource::PrimitiveTopology,
    tasks::{AsyncComputeTaskPool, Task, block_on, futures_lite::future},
};
use voxel_world::{AIR, CHUNK_SIZE, ChunkNeighborhood, VoxelWorld, block_color};

const MAX_JOBS: usize = 16;
const STARTS_PER_FRAME: usize = 8;
const UPLOADS_PER_FRAME: usize = 8;
const UPLOAD_BYTES_PER_FRAME: usize = 8 * 1024 * 1024;
type Stamp = [Option<u64>; 7];

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

/// Greedily merges coplanar exposed faces of the same opaque material.
/// Coordinates are chunk-local; only the six axial neighbor chunks are sampled.
/// Scratch space is one fixed 32x32 mask; output is bounded by six faces per voxel.
pub fn greedy_mesh(neighborhood: &ChunkNeighborhood) -> MeshData {
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
                        let block = neighborhood.block(cell);
                        mask[u + v * size] =
                            if block != AIR && neighborhood.block(cell + normal) == AIR {
                                block
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
        let empty = world.chunks[&coord].is_empty();
        let task = AsyncComputeTaskPool::get().spawn(async move {
            if empty {
                MeshData::default()
            } else {
                greedy_mesh(&neighborhood)
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
    use voxel_world::{CHUNK_VOLUME, Chunk, DIRT, STONE, index};

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

    // Expand each quad into its unit faces: detects gaps, duplicates, incorrect material
    // merges, bad boundary ownership and flipped geometry, not just a triangle count.
    fn assert_surface(world: &VoxelWorld, coord: IVec3) -> MeshData {
        let neighborhood = world.neighborhood(coord);
        let mesh = greedy_mesh(&neighborhood);
        let mut expected = HashSet::new();
        for y in 0..CHUNK_SIZE {
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let cell = IVec3::new(x, y, z);
                    let material = neighborhood.block(cell);
                    if material == AIR {
                        continue;
                    }
                    for axis in 0..3 {
                        for sign in [-1, 1] {
                            let mut normal = IVec3::ZERO;
                            normal[axis] = sign;
                            if neighborhood.block(cell + normal) == AIR {
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
            let material = (1..=5)
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
    fn solid_chunk_merges_to_six_outward_quads() {
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, chunk_with(|_| STONE));
        assert_eq!(assert_surface(&world, IVec3::ZERO).indices.len(), 36);
    }

    #[test]
    fn material_seams_do_not_merge_and_internal_solids_do_not_draw() {
        let mut world = VoxelWorld::default();
        world.insert(
            IVec3::ZERO,
            chunk_with(|p| if p.x < 16 { STONE } else { DIRT }),
        );
        assert_eq!(assert_surface(&world, IVec3::ZERO).indices.len(), 60);
    }

    #[test]
    fn axial_neighbors_occlude_and_removal_restores_boundary_faces() {
        let mut world = VoxelWorld::default();
        let coord = IVec3::new(-2, 0, -1);
        world.insert(coord, chunk_with(|_| STONE));
        for offset in [
            IVec3::X,
            IVec3::NEG_X,
            IVec3::Y,
            IVec3::NEG_Y,
            IVec3::Z,
            IVec3::NEG_Z,
        ] {
            world.insert(coord + offset, chunk_with(|_| DIRT));
        }
        assert!(assert_surface(&world, coord).indices.is_empty());
        world.remove(coord + IVec3::NEG_X);
        assert_eq!(assert_surface(&world, coord).indices.len(), 6);
    }

    #[test]
    fn irregular_cavity_and_chunk_edges_have_exact_face_coverage() {
        let mut world = VoxelWorld::default();
        world.insert(
            IVec3::ZERO,
            chunk_with(|p| {
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
        assert_surface(&world, IVec3::ZERO);
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
            ready: Some(greedy_mesh(&neighborhood)),
        }
    }

    #[test]
    fn neighbor_change_rejects_completed_mesh_before_upload() {
        let mut app = renderer_app();
        app.world_mut()
            .resource_mut::<VoxelWorld>()
            .insert(IVec3::ZERO, chunk_with(|_| STONE));
        let job = ready_job(app.world().resource::<VoxelWorld>(), IVec3::ZERO);
        app.world_mut().resource_mut::<Renderer>().jobs.push(job);
        app.world_mut()
            .resource_mut::<VoxelWorld>()
            .insert(IVec3::X, chunk_with(|_| STONE));
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
            .insert(IVec3::ZERO, chunk_with(|_| STONE));
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
