//! Bounded headless GPU physics slice. Positions are cube centers in world coordinates.
//! Bodies stay resident between steps. One submission/readback may be in flight;
//! `try_readback` polls without waiting. Terrain changes and body edits require idle state.
//! GPU-built uniform grid broadphase with unbounded per-cell linked lists.
use bytemuck::{Pod, Zeroable};
use std::sync::mpsc::{self, Receiver};
use wgpu::util::DeviceExt;

pub const MAX_BODIES: usize = 1024;
/// 0.2 seconds of supported low-speed motion at 120 Hz.
pub const SETTLE_TICKS: u32 = 24;
pub const MAX_PLAYERS: usize = 16;
const SOLVER_ITERATIONS: usize = 4;
/// Maximum walking force, shared across all bodies touched by one player each substep.
pub const PLAYER_PUSH_FORCE: f32 = 60.;
/// Fixed window used to convert contact impulse to an attachment load / pressure.
pub const CONTACT_DT: f32 = 1. / 120.;
/// Maximum terrain events retained per GPU submission and in the CPU drain queue.
pub const MAX_TERRAIN_CONTACTS: usize = 8192;
const BODY_BYTES: usize = std::mem::size_of::<Body>();
const EVENT_OFFSET: usize = MAX_BODIES * BODY_BYTES;
const EVENT_BYTES: usize = 16 + MAX_TERRAIN_CONTACTS * 32;
/// Fixed event payload copied with each completed batch, including its header.
pub const TERRAIN_EVENT_READBACK_BYTES: usize = EVENT_BYTES;

/// Authored properties for metre cubes. Density and fracture resistance are independent.
#[derive(Clone, Copy, Debug)]
pub struct Material {
    pub density: f32,
    pub damage_onset: f32,
    pub fracture_budget: f32,
    pub fracture_efficiency: f32,
    pub attachment_strength: f32,
    pub restitution: f32,
    pub friction: f32,
}
impl Material {
    /// Pressure-gated fraction of dissipated contact work, in joules.
    pub fn damage_energy(&self, dissipated_energy: f32, force: f32, area: f32) -> f32 {
        if !dissipated_energy.is_finite()
            || !force.is_finite()
            || !area.is_finite()
            || dissipated_energy <= 0.
            || force <= 0.
            || area <= 0.
        {
            return 0.;
        }
        let t = (force / area / self.damage_onset - 1.).clamp(0., 1.);
        dissipated_energy * self.fracture_efficiency * t * t * (3. - 2. * t)
    }
    pub fn fracture_limit(&self, volume: f32) -> f32 {
        self.fracture_budget * volume.max(0.)
    }
}
const MATERIALS: [Material; 6] = [
    Material {
        density: 1.,
        damage_onset: 2000.,
        fracture_budget: 60.,
        fracture_efficiency: 0.5,
        attachment_strength: 1500.,
        restitution: 0.,
        friction: 8.,
    },
    Material {
        density: 1.,
        damage_onset: 1000.,
        fracture_budget: 24.,
        fracture_efficiency: 0.5,
        attachment_strength: 1000.,
        restitution: 0.05,
        friction: 8.,
    },
    Material {
        density: 1.5,
        damage_onset: 1200.,
        fracture_budget: 36.,
        fracture_efficiency: 0.5,
        attachment_strength: 1200.,
        restitution: 0.05,
        friction: 8.,
    },
    Material {
        density: 3.,
        damage_onset: 2000.,
        fracture_budget: 60.,
        fracture_efficiency: 0.5,
        attachment_strength: 1500.,
        restitution: 0.1,
        friction: 6.,
    },
    Material {
        density: 1.2,
        damage_onset: 800.,
        fracture_budget: 18.,
        fracture_efficiency: 0.5,
        attachment_strength: 1000.,
        restitution: 0.,
        friction: 10.,
    },
    Material {
        density: 0.7,
        damage_onset: 1500.,
        fracture_budget: 90.,
        fracture_efficiency: 0.5,
        attachment_strength: 1200.,
        restitution: 0.15,
        friction: 5.,
    },
];
/// Unknown identifiers use the inert/default material at index zero.
pub fn material(id: u32) -> &'static Material {
    MATERIALS.get(id as usize).unwrap_or(&MATERIALS[0])
}

/// Work already assigned to the terrain half of a contact, before pressure gating.
/// Force is the voxel's share of total load; area is its overlapping contact face.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct TerrainContact {
    pub target: [i32; 3],
    pub material: u32,
    pub dissipated_energy: f32,
    pub force: f32,
    pub area: f32,
}
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RawTerrainContact {
    contact: TerrainContact,
    padding: f32,
}

// Generate shader constants from the Rust table, avoiding a second material database.
fn shader_source() -> String {
    let mut source = String::from(
        "struct Material { density: f32, damage_onset: f32, fracture_budget: f32, fracture_efficiency: f32, attachment_strength: f32, restitution: f32, friction: f32 }\nconst MATERIALS = array<Material, 6>(\n",
    );
    for m in MATERIALS {
        source.push_str(&format!(
            "Material({:?},{:?},{:?},{:?},{:?},{:?},{:?}),\n",
            m.density,
            m.damage_onset,
            m.fracture_budget,
            m.fracture_efficiency,
            m.attachment_strength,
            m.restitution,
            m.friction
        ));
    }
    source.push_str(&format!(");\nconst PLAYER_PUSH_FORCE: f32 = {PLAYER_PUSH_FORCE:?};\nconst CONTACT_DT: f32 = {CONTACT_DT:?};\n"));
    source.push_str(include_str!("physics.wgsl"));
    source
}

/// Kinematic player AABB: feet position, horizontal half-width 0.3, height 1.8.
/// Walking transfers force-limited momentum; positions are held for a batch.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct PlayerCollider {
    pub position: [f32; 3],
    pub id: u32,
    pub velocity: [f32; 3],
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct Body {
    pub position: [f32; 3],
    /// Zero disables a slot; 1..=5 are grass, dirt, stone, sand, wood.
    pub material: u32,
    pub velocity: [f32; 3],
    /// Low 16 bits: whole-joule diagnostic; high 16: stable substeps.
    pub damage_sleep: u32,
    pub fracture_damage: f32,
    pub padding: [f32; 3],
}
impl Body {
    pub fn new(position: [f32; 3], material: u32) -> Self {
        Self {
            position,
            material,
            ..Self::default()
        }
    }
    pub fn settling_candidate(&self) -> bool {
        self.material != 0 && self.damage_sleep >> 16 >= SETTLE_TICKS
    }
    pub fn damage(&self) -> u32 {
        self.damage_sleep & 65535
    }
    pub fn damage_joules(&self) -> f32 {
        self.fracture_damage
    }
    pub fn set_damage_joules(&mut self, joules: f32) {
        self.fracture_damage = if joules.is_finite() {
            joules.max(0.)
        } else {
            0.
        };
        self.damage_sleep =
            (self.damage_sleep & 0xffff0000) | (self.fracture_damage.min(65535.) as u32);
    }
    pub fn destroyed(&self) -> bool {
        self.material != 0 && self.fracture_damage >= material(self.material).fracture_limit(1.)
    }
}

/// Dense x-fastest, then z, then y cells. Outside this bounded region is solid.
#[derive(Clone, Debug)]
pub struct Terrain {
    pub origin: [i32; 3],
    pub size: [u32; 3],
    pub cells: Vec<u32>,
}
impl Terrain {
    pub fn index(&self, x: u32, y: u32, z: u32) -> usize {
        (x + self.size[0] * (z + self.size[2] * y)) as usize
    }
    fn valid(&self) -> bool {
        self.size.iter().all(|&n| n > 0 && n <= 128)
            && self.cells.len() == self.size.iter().map(|&n| n as usize).product::<usize>()
            && self.cells.iter().all(|&material| material <= 5)
    }
}
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    origin: [i32; 3],
    count: u32,
    size: [u32; 3],
    dt: f32,
    player_count: u32,
    padding: [u32; 3],
}

pub struct GpuPhysics {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pub adapter_name: String,
    bodies: [wgpu::Buffer; 2],
    impulses: wgpu::Buffer,
    players: wgpu::Buffer,
    player_count: usize,
    terrain: wgpu::Buffer,
    params: wgpu::Buffer,
    readback: wgpu::Buffer,
    terrain_events: wgpu::Buffer,
    terrain_contacts: Vec<TerrainContact>,
    /// Cumulative discarded events, including an undrained CPU queue overflowing.
    pub terrain_contact_overflow: u64,
    groups: [wgpu::BindGroup; 2],
    integrate: wgpu::ComputePipeline,
    player_motor: wgpu::ComputePipeline,
    contacts: wgpu::ComputePipeline,
    pair_contacts: wgpu::ComputePipeline,
    pair_budget: wgpu::ComputePipeline,
    finalize: wgpu::ComputePipeline,
    clear_grid: wgpu::ComputePipeline,
    build_grid: wgpu::ComputePipeline,
    grid_count: u32,
    terrain_info: Terrain,
    count: usize,
    pending_impulses: Vec<[f32; 4]>,
    pending: Option<Receiver<Result<(), wgpu::BufferAsyncError>>>,
}
impl GpuPhysics {
    pub fn new(terrain: Terrain) -> Result<Self, String> {
        pollster::block_on(Self::new_async(terrain))
    }
    pub async fn new_async(terrain_info: Terrain) -> Result<Self, String> {
        if !terrain_info.valid() {
            return Err("terrain dimensions must be 1..=128 with matching cells".into());
        }
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?;
        let adapter_name = format!(
            "{} ({:?})",
            adapter.get_info().name,
            adapter.get_info().device_type
        );
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("voxel physics"),
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?;
        let buffer = |label, size, usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let body_usage = wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::COPY_SRC;
        let bodies = [
            buffer("bodies", (MAX_BODIES * BODY_BYTES) as u64, body_usage),
            buffer(
                "predicted bodies",
                (MAX_BODIES * BODY_BYTES) as u64,
                body_usage,
            ),
        ];
        let impulses = buffer(
            "impulses",
            (MAX_BODIES * 16) as u64,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        let players = buffer(
            "kinematic players",
            (MAX_PLAYERS * 32) as u64,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        let terrain = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("terrain"),
            contents: bytemuck::cast_slice(&terrain_info.cells),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });
        let params = buffer(
            "parameters",
            48,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let readback = buffer(
            "snapshot",
            (EVENT_OFFSET + EVENT_BYTES) as u64,
            wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        );
        let terrain_events = buffer("terrain contact events", EVENT_BYTES as u64, body_usage);
        let grid_count = terrain_info
            .size
            .iter()
            .map(|n| n.div_ceil(2))
            .product::<u32>();
        let grid_heads = buffer(
            "grid heads",
            u64::from(grid_count) * 4,
            wgpu::BufferUsages::STORAGE,
        );
        let grid_next = buffer(
            "grid next",
            (MAX_BODIES * 4) as u64,
            wgpu::BufferUsages::STORAGE,
        );
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("voxel physics"),
            source: wgpu::ShaderSource::Wgsl(shader_source().into()),
        });
        let entries: Vec<_> = (0..9)
            .map(|binding| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: if binding == 4 {
                        wgpu::BufferBindingType::Uniform
                    } else {
                        wgpu::BufferBindingType::Storage {
                            read_only: binding == 0 || binding == 2 || binding == 7,
                        }
                    },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            })
            .collect();
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &entries,
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = |entry| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                module: &shader,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let integrate = pipeline("integrate");
        let player_motor = pipeline("player_motor");
        let contacts = pipeline("contacts");
        let finalize = pipeline("finalize");
        let clear_grid = pipeline("clear_grid");
        let build_grid = pipeline("build_grid");
        let pair_contacts = pipeline("pair_contacts");
        let pair_budget = pipeline("pair_budget");
        let group = |src: usize, dst: usize| {
            let buffers = [
                &bodies[src],
                &bodies[dst],
                &terrain,
                &impulses,
                &params,
                &grid_heads,
                &grid_next,
                &players,
                &terrain_events,
            ];
            let entries: Vec<_> = buffers
                .iter()
                .enumerate()
                .map(|(i, b)| wgpu::BindGroupEntry {
                    binding: i as u32,
                    resource: b.as_entire_binding(),
                })
                .collect();
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &layout,
                entries: &entries,
            })
        };
        let groups = [group(0, 1), group(1, 0)];
        Ok(Self {
            device,
            queue,
            adapter_name,
            bodies,
            impulses,
            players,
            player_count: 0,
            terrain,
            params,
            readback,
            groups,
            terrain_events,
            terrain_contacts: Vec::new(),
            terrain_contact_overflow: 0,
            player_motor,
            integrate,
            contacts,
            pair_contacts,
            pair_budget,
            finalize,
            clear_grid,
            build_grid,
            grid_count,
            terrain_info,
            count: 0,
            pending_impulses: vec![[0.; 4]; MAX_BODIES],
            pending: None,
        })
    }
    pub fn is_busy(&self) -> bool {
        self.pending.is_some()
    }
    /// Upload bounded kinematic input while idle. No additional GPU readback.
    pub fn set_players(&mut self, players: &[PlayerCollider]) -> Result<(), String> {
        if self.is_busy() {
            return Err("physics submission in flight".into());
        }
        if players.len() > MAX_PLAYERS
            || players
                .iter()
                .any(|p| !p.position.iter().chain(&p.velocity).all(|v| v.is_finite()))
        {
            return Err("invalid players or capacity exceeded".into());
        }
        self.player_count = players.len();
        if !players.is_empty() {
            self.queue
                .write_buffer(&self.players, 0, bytemuck::cast_slice(players));
        }
        Ok(())
    }
    /// Replace the active slot list only when spawning/removing; do not upload every tick.
    pub fn set_bodies(&mut self, bodies: &[Body]) -> Result<(), String> {
        if self.is_busy() {
            return Err("physics submission in flight".into());
        }
        if bodies.len() > MAX_BODIES
            || bodies.iter().any(|b| {
                b.material > 5
                    || !b.position.iter().chain(&b.velocity).all(|v| v.is_finite())
                    || !b.fracture_damage.is_finite()
                    || b.fracture_damage < 0.
            })
        {
            return Err("invalid bodies or capacity exceeded".into());
        }
        self.count = bodies.len();
        self.pending_impulses.fill([0.; 4]);
        if !bodies.is_empty() {
            self.queue
                .write_buffer(&self.bodies[0], 0, bytemuck::cast_slice(bodies));
        }
        Ok(())
    }
    /// Same-size terrain update, called only following authoritative voxel edits.
    pub fn update_terrain(&mut self, cells: &[u32]) -> Result<(), String> {
        if self.is_busy() {
            return Err("physics submission in flight".into());
        }
        if cells.len() != self.terrain_info.cells.len() {
            return Err("terrain size mismatch".into());
        }
        if cells.iter().any(|&material| material > 5) {
            return Err("invalid terrain material".into());
        }
        self.queue
            .write_buffer(&self.terrain, 0, bytemuck::cast_slice(cells));
        self.terrain_info.cells.copy_from_slice(cells);
        Ok(())
    }
    /// Patch a contiguous range of x-fastest terrain cells while idle.
    /// Validation precedes all writes; uploads and CPU copies touch only the range.
    pub fn update_terrain_range(&mut self, start: usize, cells: &[u32]) -> Result<(), String> {
        if self.is_busy() {
            return Err("physics submission in flight".into());
        }
        let end = start
            .checked_add(cells.len())
            .ok_or("terrain range overflow")?;
        if end > self.terrain_info.cells.len() {
            return Err("terrain range out of bounds".into());
        }
        if cells.iter().any(|&material| material > 5) {
            return Err("invalid terrain material".into());
        }
        if !cells.is_empty() {
            self.queue.write_buffer(
                &self.terrain,
                (start * 4) as u64,
                bytemuck::cast_slice(cells),
            );
            self.terrain_info.cells[start..end].copy_from_slice(cells);
        }
        Ok(())
    }
    /// Queue momentum input in kg*m/s. Can also queue while an earlier tick runs.
    pub fn impulse(&mut self, slot: usize, impulse: [f32; 3]) -> Result<(), String> {
        if slot >= self.count || !impulse.iter().all(|v| v.is_finite() && v.abs() <= 10000.) {
            return Err("invalid impulse".into());
        }
        for (axis, value) in impulse.into_iter().enumerate() {
            self.pending_impulses[slot][axis] += value;
        }
        Ok(())
    }
    /// Submit 1..=8 substeps, each at most 1/120s. Returns false under backpressure.
    pub fn submit(&mut self, dt: f32, substeps: u32) -> Result<bool, String> {
        if self.is_busy() || self.count == 0 {
            return Ok(false);
        }
        if !(dt > 0. && dt <= 1. / 120.) || !(1..=8).contains(&substeps) {
            return Err("invalid substep parameters".into());
        }
        let params = Params {
            origin: self.terrain_info.origin,
            size: self.terrain_info.size,
            count: self.count as u32,
            dt,
            player_count: self.player_count as u32,
            padding: [0; 3],
        };
        self.queue
            .write_buffer(&self.params, 0, bytemuck::bytes_of(&params));
        if self.pending_impulses[..self.count]
            .iter()
            .any(|v| *v != [0.; 4])
        {
            self.queue.write_buffer(
                &self.impulses,
                0,
                bytemuck::cast_slice(&self.pending_impulses[..self.count]),
            );
            self.pending_impulses.fill([0.; 4]);
        }
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.clear_buffer(&self.terrain_events, 0, None);
        for _ in 0..substeps {
            let mut dispatch = |pipeline: &wgpu::ComputePipeline, group: usize, count: u32| {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &self.groups[group], &[]);
                pass.dispatch_workgroups(count.div_ceil(64), 1, 1);
            };
            dispatch(&self.player_motor, 0, 1);
            dispatch(&self.integrate, 0, self.count as u32);
            for _ in 0..SOLVER_ITERATIONS {
                dispatch(&self.clear_grid, 1, self.grid_count);
                dispatch(&self.build_grid, 1, self.count as u32);
                dispatch(&self.pair_contacts, 1, self.count as u32);
                dispatch(&self.pair_budget, 1, 1);
                dispatch(&self.contacts, 0, self.count as u32);
            }
            dispatch(&self.clear_grid, 1, self.grid_count);
            dispatch(&self.build_grid, 1, self.count as u32);
            dispatch(&self.finalize, 1, self.count as u32);
        }
        encoder.copy_buffer_to_buffer(
            &self.bodies[0],
            0,
            &self.readback,
            0,
            (self.count * BODY_BYTES) as u64,
        );
        encoder.copy_buffer_to_buffer(
            &self.terrain_events,
            0,
            &self.readback,
            EVENT_OFFSET as u64,
            EVENT_BYTES as u64,
        );
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = mpsc::channel();
        self.readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
        self.pending = Some(rx);
        Ok(true)
    }
    pub fn try_readback(&mut self) -> Result<Option<Vec<Body>>, String> {
        if self.pending.is_none() {
            return Ok(None);
        }
        self.device
            .poll(wgpu::PollType::Poll)
            .map_err(|e| e.to_string())?;
        let result = match self.pending.as_ref().unwrap().try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        self.pending = None;
        result.map_err(|e| e.to_string())?;
        let mapped = self.readback.slice(..).get_mapped_range();
        let bodies = bytemuck::cast_slice(&mapped[..self.count * BODY_BYTES]).to_vec();
        let attempted =
            u32::from_ne_bytes(mapped[EVENT_OFFSET..EVENT_OFFSET + 4].try_into().unwrap()) as usize;
        let available = attempted.min(MAX_TERRAIN_CONTACTS);
        let retained = available.min(MAX_TERRAIN_CONTACTS - self.terrain_contacts.len());
        let events: &[RawTerrainContact] =
            bytemuck::cast_slice(&mapped[EVENT_OFFSET + 16..EVENT_OFFSET + 16 + retained * 32]);
        self.terrain_contacts
            .extend(events.iter().map(|event| event.contact));
        self.terrain_contact_overflow += (attempted - retained) as u64;
        drop(mapped);
        self.readback.unmap();
        Ok(Some(bodies))
    }
    /// Drain bounded terrain work after a successful body readback.
    pub fn take_terrain_contacts(&mut self) -> Vec<TerrainContact> {
        std::mem::take(&mut self.terrain_contacts)
    }
    /// Blocking only for benchmarks and tests, never call this from a server tick.
    pub fn wait_readback(&mut self) -> Result<Option<Vec<Body>>, String> {
        if self.pending.is_some() {
            self.device
                .poll(wgpu::PollType::Wait)
                .map_err(|e| e.to_string())?;
        }
        self.try_readback()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Mesa's software adapter may crash during concurrent instance initialization.
    static GPU_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());
    #[test]
    fn layouts() {
        assert_eq!(std::mem::size_of::<Body>(), 48);
        assert_eq!(std::mem::size_of::<RawTerrainContact>(), 32);
        assert_eq!(std::mem::size_of::<Params>(), 48);
        assert_eq!(std::mem::size_of::<PlayerCollider>(), 32);
    }
    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn falls_collides_and_settles() {
        let _guard = GPU_TEST.lock().unwrap();
        let mut terrain = Terrain {
            origin: [0; 3],
            size: [8; 3],
            cells: vec![0; 512],
        };
        for z in 0..8 {
            for x in 0..8 {
                let i = terrain.index(x, 0, z);
                terrain.cells[i] = 3;
            }
        }
        let mut gpu = GpuPhysics::new(terrain).unwrap();
        gpu.set_bodies(&[Body::new([3.5, 4.5, 3.5], 3)]).unwrap();
        let mut last = vec![];
        for _ in 0..180 {
            assert!(gpu.submit(1. / 120., 2).unwrap());
            last = gpu.wait_readback().unwrap().unwrap();
        }
        assert!((last[0].position[1] - 1.5).abs() < 0.01, "{:?}", last);
        assert!(last[0].settling_candidate(), "{:?}", last);
        let damage = last[0].damage();
        for _ in 0..60 {
            gpu.submit(1. / 120., 2).unwrap();
            last = gpu.wait_readback().unwrap().unwrap();
        }
        assert_eq!(
            last[0].damage(),
            damage,
            "support must not damage terrain/body"
        );
        gpu.impulse(0, [4., 8., 0.]).unwrap();
        gpu.submit(1. / 120., 2).unwrap();
        last = gpu.wait_readback().unwrap().unwrap();
        assert!(last[0].velocity[1] > 0.);
    }
    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn pair_momentum_damage_and_async_backpressure() {
        let _guard = GPU_TEST.lock().unwrap();
        let terrain = Terrain {
            origin: [0; 3],
            size: [16; 3],
            cells: vec![0; 4096],
        };
        let mut gpu = GpuPhysics::new(terrain).unwrap();
        // Predicted centers straddle the width-two cell boundary at x=8.
        let mut a = Body::new([7.45, 8.5, 8.5], 3);
        a.velocity = [6., 0., 0.];
        let mut b = Body::new([8.55, 8.5, 8.5], 3);
        b.velocity = [-6., 0., 0.];
        gpu.set_bodies(&[a, b]).unwrap();
        gpu.submit(1. / 120., 2).unwrap();
        assert!(!gpu.submit(1. / 120., 2).unwrap());
        assert!(gpu.set_bodies(&[]).is_err());
        assert!(gpu.update_terrain_range(0, &[3]).is_err());
        let start = std::time::Instant::now();
        let result = loop {
            if let Some(result) = gpu.try_readback().unwrap() {
                break result;
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(10));
            std::thread::yield_now();
        };
        assert!((result[0].velocity[0] + result[1].velocity[0]).abs() < 0.001);
        assert!(
            result[0].velocity[0] < 0. && result[1].velocity[0] > 0.,
            "{result:?}"
        );
        assert!(
            result[0].damage() > 0 && result[1].damage() > 0,
            "{result:?}"
        );
    }
    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn terrain_range_validation_and_gpu_visibility() {
        let _guard = GPU_TEST.lock().unwrap();
        let mut gpu = GpuPhysics::new(Terrain {
            origin: [0; 3],
            size: [8; 3],
            cells: vec![0; 512],
        })
        .unwrap();
        assert!(gpu.update_terrain_range(usize::MAX, &[3]).is_err());
        assert!(gpu.update_terrain_range(512, &[3]).is_err());
        gpu.update_terrain_range(512, &[]).unwrap();
        // Insert one floor voxel beneath the cube; no full terrain upload.
        let index = gpu.terrain_info.index(3, 0, 3);
        gpu.update_terrain_range(index, &[3]).unwrap();
        assert_eq!(
            gpu.terrain_info.cells.iter().filter(|&&c| c != 0).count(),
            1
        );
        gpu.set_bodies(&[Body::new([3.5, 1.5, 3.5], 3)]).unwrap();
        gpu.submit(1. / 120., 8).unwrap();
        let bodies = gpu.wait_readback().unwrap().unwrap();
        assert!((bodies[0].position[1] - 1.5).abs() < 0.001);
        gpu.update_terrain_range(index, &[0]).unwrap();
        gpu.submit(1. / 120., 8).unwrap();
        let bodies = gpu.wait_readback().unwrap().unwrap();
        assert!(bodies[0].position[1] < 1.49);
    }
    fn floor_gpu() -> GpuPhysics {
        let mut terrain = Terrain {
            origin: [0; 3],
            size: [16; 3],
            cells: vec![0; 4096],
        };
        terrain.cells[..256].fill(3);
        GpuPhysics::new(terrain).unwrap()
    }

    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn player_push_clearance_and_terrain() {
        let _guard = GPU_TEST.lock().unwrap();
        let mut gpu = floor_gpu();
        let player = PlayerCollider {
            position: [4., 1., 4.],
            velocity: [4., 0., 0.],
            id: 7,
            padding: 0,
        };
        assert!(gpu.set_players(&[player; MAX_PLAYERS + 1]).is_err());
        let mut invalid = player;
        invalid.position[0] = f32::NAN;
        assert!(gpu.set_players(&[invalid]).is_err());
        gpu.set_players(&[player]).unwrap();
        gpu.set_bodies(&[Body::new([4.7, 1.5, 4.], 3)]).unwrap();
        gpu.submit(1. / 120., 1).unwrap();
        assert!(gpu.set_players(&[]).is_err());
        let bodies = gpu.wait_readback().unwrap().unwrap();
        assert!(
            bodies[0].position[0] > 4.7 && bodies[0].position[0] < 4.71,
            "{bodies:?}"
        );
        assert!(
            bodies[0].velocity[0] > 0.
                && bodies[0].velocity[0] <= PLAYER_PUSH_FORCE * CONTACT_DT / material(3).density,
            "{bodies:?}"
        );
        // A falling block lands on the player's head without adding support damage.
        gpu.set_players(&[PlayerCollider {
            velocity: [0.; 3],
            ..player
        }])
        .unwrap();
        gpu.set_bodies(&[Body::new([4., 5., 4.], 3)]).unwrap();
        let mut bodies = vec![];
        for _ in 0..40 {
            gpu.submit(1. / 120., 6).unwrap();
            bodies = gpu.wait_readback().unwrap().unwrap();
        }
        assert!((bodies[0].position[1] - 3.3).abs() < 0.005, "{bodies:?}");
        assert!(
            bodies[0].velocity.iter().all(|v| v.abs() < 0.01),
            "{bodies:?}"
        );
        let damage = bodies[0].damage();
        gpu.submit(1. / 120., 8).unwrap();
        assert_eq!(gpu.wait_readback().unwrap().unwrap()[0].damage(), damage);
        // A player cannot project a body through the region's solid boundary.
        gpu.set_players(&[PlayerCollider {
            position: [14.9, 1., 4.],
            ..player
        }])
        .unwrap();
        gpu.set_bodies(&[Body::new([15.5, 1.5, 4.], 3)]).unwrap();
        gpu.submit(1. / 120., 8).unwrap();
        let bodies = gpu.wait_readback().unwrap().unwrap();
        assert!(bodies[0].position[0] <= 15.5002, "{bodies:?}");
        assert!(bodies[0].velocity[0].abs() < 0.01, "{bodies:?}");
        assert_eq!(
            bodies[0].damage(),
            0,
            "a pinned walking push must not manufacture impact energy"
        );
        // At the velocity clamp, a body cannot cross the player's 1.6 m expanded width.
        gpu.set_players(&[PlayerCollider {
            velocity: [0.; 3],
            ..player
        }])
        .unwrap();
        let mut fast = Body::new([2.9, 1.5, 4.], 3);
        fast.velocity = [30., 0., 0.];
        gpu.set_bodies(&[fast]).unwrap();
        gpu.submit(1. / 120., 8).unwrap();
        let bodies = gpu.wait_readback().unwrap().unwrap();
        assert!(bodies[0].position[0] <= 3.201, "{bodies:?}");
    }

    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn stack_settles_without_air_freezing() {
        let _guard = GPU_TEST.lock().unwrap();
        let mut gpu = floor_gpu();
        let bodies: Vec<_> = (0..4)
            .map(|i| Body::new([4.5, 1.5 + i as f32, 4.5], 3))
            .collect();
        gpu.set_bodies(&bodies).unwrap();
        let mut result = vec![];
        for _ in 0..60 {
            gpu.submit(1. / 120., 6).unwrap();
            result = gpu.wait_readback().unwrap().unwrap();
        }
        for (i, b) in result.iter().enumerate() {
            assert!(
                (b.position[1] - (1.5 + i as f32)).abs() < 0.08,
                "{result:?}"
            );
            assert!(b.velocity.iter().all(|v| v.abs() < 0.35), "{result:?}");
            assert!(b.settling_candidate(), "{result:?}");
            assert_eq!(b.damage(), 0, "{result:?}");
        }
        // Remove the floor, preserving resident body state and old support counters.
        gpu.update_terrain_range(0, &[0; 256]).unwrap();
        gpu.submit(1. / 120., 8).unwrap();
        let falling = gpu.wait_readback().unwrap().unwrap();
        assert!(
            falling[0].position[1] < result[0].position[1] - 0.015,
            "{falling:?}"
        );
        assert!(
            falling.iter().all(|b| !b.settling_candidate()),
            "{falling:?}"
        );
    }

    #[test]
    fn material_damage_accumulates_independently_of_mass() {
        let stone = material(3);
        assert_eq!(stone.fracture_limit(1.), 60.);
        assert_eq!(stone.damage_energy(12., 400., 0.01), 6.);
        assert!(400. < stone.attachment_strength);
        assert_eq!(stone.damage_energy(12., 400., 1.), 0.);
        assert_eq!(stone.damage_energy(12., 2000., 1.), 0.);
        assert_eq!(stone.damage_energy(12., 3000., 1.), 3.);
        assert_eq!(stone.damage_energy(12., 4000., 1.), 6.);
        let mut body = Body::new([0.; 3], 3);
        body.damage_sleep = SETTLE_TICKS << 16;
        for _ in 0..9 {
            body.set_damage_joules(body.damage_joules() + stone.damage_energy(12., 400., 0.01));
            assert!(!body.destroyed());
        }
        body.set_damage_joules(body.damage_joules() + 6.);
        assert!(body.destroyed());
        assert!(body.settling_candidate());
        let lighter = Material {
            density: 0.1,
            ..*stone
        };
        assert_eq!(lighter.fracture_limit(1.), stone.fracture_limit(1.));
        assert_eq!(lighter.damage_energy(12., 400., 0.01), 6.);
    }

    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn walking_budget_is_mass_sensitive_and_shared() {
        let _guard = GPU_TEST.lock().unwrap();
        let mut gpu = floor_gpu();
        gpu.set_players(&[PlayerCollider {
            position: [4., 5., 4.],
            velocity: [4., 0., 0.],
            ..Default::default()
        }])
        .unwrap();
        let mut speeds = Vec::new();
        for mat in [1, 3] {
            gpu.set_bodies(&[Body::new([4.8, 5.5, 4.], mat)]).unwrap();
            gpu.submit(CONTACT_DT, 1).unwrap();
            let bodies = gpu.wait_readback().unwrap().unwrap();
            let speed = bodies[0].velocity[0];
            assert!(
                (speed * material(mat).density - PLAYER_PUSH_FORCE * CONTACT_DT).abs() < 0.0001
            );
            assert!((bodies[0].position[0] - 4.8 - speed * CONTACT_DT).abs() < 0.0001);
            assert_eq!(bodies[0].damage_joules(), 0.);
            speeds.push(speed);
        }
        assert!((speeds[0] / speeds[1] - 3.).abs() < 0.001);
        // Two vertically adjacent bodies touched by the same player's tall AABB.
        gpu.set_bodies(&[Body::new([4.8, 5.4, 4.], 3), Body::new([4.8, 6.4, 4.], 3)])
            .unwrap();
        gpu.submit(CONTACT_DT, 1).unwrap();
        let bodies = gpu.wait_readback().unwrap().unwrap();
        let momentum: f32 = bodies
            .iter()
            .map(|b| b.velocity[0] * material(b.material).density)
            .sum();
        assert!(
            (momentum - PLAYER_PUSH_FORCE * CONTACT_DT).abs() < 0.0001,
            "{bodies:?}"
        );
        assert!(gpu.take_terrain_contacts().is_empty());
        gpu.set_bodies(&[Body::new([4.7, 5.5, 4.], 3)]).unwrap();
        gpu.submit(CONTACT_DT, 8).unwrap();
        let bodies = gpu.wait_readback().unwrap().unwrap();
        assert!((bodies[0].velocity[0] * 3. - 8. * PLAYER_PUSH_FORCE * CONTACT_DT).abs() < 0.0001);
    }

    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn terrain_work_is_split_and_support_is_quiet() {
        let _guard = GPU_TEST.lock().unwrap();
        let mut gpu = floor_gpu();
        eprintln!("terrain work adapter: {}", gpu.adapter_name);
        // A centred face overlaps four floor voxels by exactly 0.25 m² each.
        let mut body = Body::new([4., 1.6, 4.], 3);
        body.velocity[1] = -12.;
        gpu.set_bodies(&[body]).unwrap();
        gpu.submit(CONTACT_DT, 1).unwrap();
        let bodies = gpu.wait_readback().unwrap().unwrap();
        let events = gpu.take_terrain_contacts();
        assert_eq!(events.len(), 4, "{events:?}");
        let incoming = 12. + 9.81 * CONTACT_DT;
        let total_dissipated = 0.5 * material(3).density * incoming * incoming * (1. - 0.1 * 0.1);
        let terrain_share: f32 = events.iter().map(|e| e.dissipated_energy).sum();
        assert!(
            (terrain_share - total_dissipated * 0.5).abs() < 0.001,
            "{events:?}"
        );
        let force: f32 = events.iter().map(|e| e.force).sum();
        assert!((force - material(3).density * incoming * 1.1 / CONTACT_DT).abs() < 0.01);
        for event in &events {
            assert_eq!(event.material, 3);
            assert_eq!(event.target[1], 0);
            assert!((event.area - 0.25).abs() < 0.0001);
        }
        let terrain_damage: f32 = events
            .iter()
            .map(|e| material(e.material).damage_energy(e.dissipated_energy, e.force, e.area))
            .sum();
        assert!((bodies[0].damage_joules() - terrain_damage).abs() < 0.001);
        assert!(bodies[0].damage_joules() > 0. && !bodies[0].destroyed());
        assert!(gpu.take_terrain_contacts().is_empty());
        assert_eq!(gpu.terrain_contact_overflow, 0);
        body = Body::new([4., 1.5, 4.], 3);
        gpu.set_bodies(&[body]).unwrap();
        for _ in 0..8 {
            gpu.submit(CONTACT_DT, 8).unwrap();
            let bodies = gpu.wait_readback().unwrap().unwrap();
            assert_eq!(bodies[0].damage_joules(), 0.);
            assert!(gpu.take_terrain_contacts().is_empty());
        }
        body = Body::new([4., 1.6, 4.], 3);
        body.velocity[1] = -20.;
        gpu.set_bodies(&[body]).unwrap();
        gpu.submit(CONTACT_DT, 1).unwrap();
        assert!(gpu.wait_readback().unwrap().unwrap()[0].destroyed());
        let initial_events = gpu.take_terrain_contacts();
        gpu.terrain_contacts
            .resize(MAX_TERRAIN_CONTACTS - 1, initial_events[0]);
        gpu.set_bodies(&[body]).unwrap();
        gpu.submit(CONTACT_DT, 1).unwrap();
        gpu.wait_readback().unwrap().unwrap();
        assert_eq!(gpu.take_terrain_contacts().len(), MAX_TERRAIN_CONTACTS);
        assert_eq!(gpu.terrain_contact_overflow, 3);
        // The GPU append counter resets at submission, independently of CPU draining.
        gpu.set_bodies(&[body]).unwrap();
        gpu.submit(CONTACT_DT, 1).unwrap();
        gpu.wait_readback().unwrap().unwrap();
        assert_eq!(gpu.take_terrain_contacts().len(), 4);
        assert_eq!(gpu.terrain_contact_overflow, 3);
    }

    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn terrain_gpu_event_capacity_is_bounded() {
        let _guard = GPU_TEST.lock().unwrap();
        let mut gpu = floor_gpu();
        gpu.set_bodies(&[Body::new([4., 4., 4.], 3)]).unwrap();
        gpu.submit(CONTACT_DT, 1).unwrap();
        gpu.wait_readback().unwrap().unwrap();
        // Exercise the production append function with more concurrent writes than slots.
        let attempts = MAX_TERRAIN_CONTACTS as u32 + 17;
        let source = format!(
            "{}\n@compute @workgroup_size(64) fn overflow_probe(@builtin(global_invocation_id) id: vec3<u32>) {{ if id.x < {attempts}u {{ emit_terrain(vec3(1,0,1),3u,10.,10000.,1.); }} }}",
            shader_source()
        );
        let shader = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("terrain overflow probe"),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
        let layout = gpu.integrate.get_bind_group_layout(0);
        let pipeline_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[&layout],
                push_constant_ranges: &[],
            });
        let pipeline = gpu
            .device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("terrain overflow probe"),
                layout: Some(&pipeline_layout),
                module: &shader,
                entry_point: Some("overflow_probe"),
                compilation_options: Default::default(),
                cache: None,
            });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        encoder.clear_buffer(&gpu.terrain_events, 0, None);
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &gpu.groups[0], &[]);
            pass.dispatch_workgroups(attempts.div_ceil(64), 1, 1);
        }
        encoder.copy_buffer_to_buffer(
            &gpu.terrain_events,
            0,
            &gpu.readback,
            EVENT_OFFSET as u64,
            EVENT_BYTES as u64,
        );
        gpu.queue.submit([encoder.finish()]);
        let (tx, rx) = mpsc::channel();
        gpu.readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        gpu.pending = Some(rx);
        gpu.wait_readback().unwrap().unwrap();
        let contacts = gpu.take_terrain_contacts();
        assert_eq!(contacts.len(), MAX_TERRAIN_CONTACTS);
        assert!(
            contacts
                .iter()
                .all(|c| c.target == [1, 0, 1] && c.dissipated_energy == 10.)
        );
        assert_eq!(gpu.terrain_contact_overflow, 17);
    }

    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn simultaneous_contacts_budget_damage_and_tiny_incoming_motion_cannot_shove() {
        let _guard = GPU_TEST.lock().unwrap();
        let mut gpu = GpuPhysics::new(Terrain {
            origin: [0; 3],
            size: [16; 3],
            cells: vec![0; 4096],
        })
        .unwrap();
        let mut bodies = [
            Body::new([5., 5., 5.], 3),
            Body::new([6., 5., 5.], 3),
            Body::new([7., 5., 5.], 3),
        ];
        bodies[0].velocity[0] = 30.;
        bodies[2].velocity[0] = -30.;
        gpu.set_bodies(&bodies).unwrap();
        gpu.submit(CONTACT_DT, 1).unwrap();
        let result = gpu.wait_readback().unwrap().unwrap();
        let kinetic = |b: &Body| {
            0.5 * material(b.material).density * b.velocity.iter().map(|v| v * v).sum::<f32>()
        };
        let initial: f32 = bodies.iter().map(kinetic).sum();
        let final_energy: f32 = result.iter().map(kinetic).sum();
        let damage: f32 = result.iter().map(Body::damage_joules).sum();
        assert!(damage > 0.);
        assert!(
            damage <= (initial - final_energy) * 0.5 + 0.02,
            "damage={damage}, lost={}",
            initial - final_energy
        );

        // A character already overlapping below a body must not get a free lift
        // from the tiny incoming velocity introduced by one gravity substep.
        gpu.set_bodies(&[Body::new([3.5, 2.5, 3.5], 3)]).unwrap();
        gpu.set_players(&[PlayerCollider {
            position: [3.5, 0.9, 3.5],
            ..Default::default()
        }])
        .unwrap();
        gpu.submit(CONTACT_DT, 1).unwrap();
        let body = gpu.wait_readback().unwrap().unwrap()[0];
        assert!(body.position[1] <= 2.501, "unbudgeted lift: {body:?}");
        assert_eq!(body.damage_joules(), 0.);
    }

    #[test]
    fn invalid_terrain_material_is_rejected_before_adapter_creation() {
        assert!(
            GpuPhysics::new(Terrain {
                origin: [0; 3],
                size: [1; 3],
                cells: vec![6]
            })
            .is_err()
        );
    }
}
