use controller::PlayerInput;
mod active_terrain;
mod actors;
#[cfg(test)]
mod blast_jump_tests;
#[cfg(test)]
mod bombardment_tests;
mod bow;
mod bow_server;
mod brains;
mod explosion;
mod health;
mod items;
mod material_damage;
#[cfg(test)]
mod noclip_tests;
mod packages;
mod physics_slice;
mod streaming;
mod terrain_stream;
use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use gameplay::{Health, Inventory};
use glam::{IVec3, Vec3};
use material_damage::{MAX_DAMAGED_BLOCKS, tool_contact};
use networking::ServerTransport;
use physics::{EYE_HEIGHT, FIXED_DT, PlayerState, look_direction, overlaps_block};
use physics_slice::PhysicsSlice;
use protocol::{
    ActorSnapshot, ClientMessage, EditRejection, InputPacket, PlayerSnapshot, ServerMessage, Snapshot,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    io,
    net::SocketAddr,
    time::Instant,
};
use terrain_stream::interests;
use voxel_world::{
    BrushEdit, CHUNK_SIZE, Chunk, MAX_CHUNK_Y, MIN_CHUNK_Y, Voxel, VoxelWorld, chunk_coord, index,
    local_coord,
};

pub use actors::{
    ActorCapabilities, ActorKind, ActorObservation, BrainKind, ObservedEntity, SimEntity, SimEvent,
};

const MAX_INPUT_QUEUE: usize = 32;
const MAX_JOURNAL_BLOCKS: usize = 1_000_000;
const MAX_QUEUED_STRIKES: usize = 4;
const STRIKE_QUEUE_TICKS: u64 = 60;
/// Pickaxe sphere: carves a smooth ~3.2 m cavity into the terrain.
const STRIKE_DIG_RADIUS: f32 = 1.6;
/// How far past the raycast surface the dig center sits, so the sphere bites
/// into the face instead of skimming it.
const STRIKE_DIG_DEPTH: f32 = 0.3;
/// Settled debris deposits a smooth mound instead of regridding one cube.
const SETTLE_RADIUS: f32 = 1.2;

type EditOutcome = Result<Option<f32>, EditRejection>;

#[derive(Clone, Copy, Debug)]
struct DamageState {
    material: u8,
    joules: f32,
    release: bool,
}

/// Ordering key for the release queue: `(y, z, x)`, matching the historical
/// `releases.sort_unstable_by_key(|(p, _)| (p.y, p.z, p.x))`.
fn release_key(pos: IVec3) -> (i32, i32, i32) {
    (pos.y, pos.z, pos.x)
}

#[derive(Clone, Copy)]
struct QueuedStrike {
    id: u64,
    request: u64,
    target: IVec3,
    revision: u64,
    expires: u64,
}
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub seed: u64,
    pub radius: i32,
    pub metrics_every: u64,
    pub gpu_physics: bool,
    pub packages: std::path::PathBuf,
    /// Spawn one `titan` hunter NPC near the spawn dummy at startup. Integration
    /// smoke tests disable it so scripted flows stay deterministic.
    pub spawn_titan: bool,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:4000".parse().unwrap(),
            seed: 7,
            radius: protocol::DEFAULT_VIEW_RADIUS,
            metrics_every: 600,
            gpu_physics: false,
            packages: game_packages::default_directory(),
            spawn_titan: true,
        }
    }
}
pub struct SimulationPlugin {
    config: ServerConfig,
    transport: parking_lot::Mutex<Option<ServerTransport>>,
    physics: parking_lot::Mutex<Option<PhysicsSlice>>,
    generator: std::sync::Arc<voxel_world::terrain::TerrainGenerator>,
    terrain: parking_lot::Mutex<Option<terrain_stream::TerrainStream>>,
}
impl SimulationPlugin {
    pub fn bind(config: ServerConfig) -> io::Result<Self> {
        let mut plugin = Self::headless(config)?;
        let transport = ServerTransport::bind(plugin.config.bind)?;
        plugin.config.bind = transport.local_addr()?;
        *plugin.transport.get_mut() = Some(transport);
        Ok(plugin)
    }

    /// Run the same authoritative simulation without socket IO, for embedding and tests.
    pub fn headless(mut config: ServerConfig) -> io::Result<Self> {
        config.radius = config.radius.clamp(1, protocol::MAX_VIEW_RADIUS);
        let generator = std::sync::Arc::new(
            game_packages::load_terrain(&config.packages)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        );
        eprintln!(
            "terrain loaded identity={} version={} seed={}",
            generator.identity(),
            generator.version(),
            config.seed
        );
        let terrain = terrain_stream::TerrainStream::new(generator.clone(), config.seed)?;
        let physics = if config.gpu_physics {
            Some(PhysicsSlice::new().map_err(io::Error::other)?)
        } else {
            None
        };
        Ok(Self {
            config,
            transport: parking_lot::Mutex::new(None),
            physics: parking_lot::Mutex::new(physics),
            generator,
            terrain: parking_lot::Mutex::new(Some(terrain)),
        })
    }
    pub fn local_addr(&self) -> SocketAddr {
        self.config.bind
    }
}
impl Plugin for SimulationPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(controller::ControllerPlugin::on(Update));
        app.configure_sets(Update, controller::ControllerSet::Intent.after(advance));
        app.add_systems(
            Update,
            observe_character_bodies.in_set(controller::ControllerSet::Intent),
        );
        let mut world = VoxelWorld {
            seed: self.config.seed,
            generator: self.generator.clone(),
            ..Default::default()
        };
        let spawn = terrain_stream::spawn_position(&self.generator, self.config.seed);
        let spawn_chunks = terrain_stream::local_chunks(spawn, 1);
        for &coord in &spawn_chunks {
            world.ensure_chunk(coord);
        }
        // Eagerly generated spawn chunks bypass the streaming worker, so seed
        // their scatter here to keep every resident chunk's entry present.
        let scatter: HashMap<IVec3, Vec<protocol::ScatterInstance>> = spawn_chunks
            .iter()
            .map(|&coord| {
                (
                    coord,
                    wire_scatter(self.generator.scatter_chunk(coord, self.config.seed)),
                )
            })
            .collect();
        let physics = self.physics.lock().take();
        app.insert_resource(world)
            .insert_resource(Simulation {
                config: self.config.clone(),
                transport: self.transport.lock().take(),
                players: HashMap::new(),
                journal: HashMap::new(),
                journal_blocks: 0,
                damage: HashMap::new(),
                release_queue: BTreeSet::new(),
                strikes: VecDeque::new(),
                arrows: Vec::new(),
                detonations: VecDeque::new(),
                next_arrow: 1,
                arrow_revision: 0,
                drops: VecDeque::new(),
                next_drop: 1,
                drop_revision: 0,
                sent_drop_revision: 0,
                packages: game_packages::PackageHost::new(&self.config.packages),
                last_needed: HashMap::new(),
                tick: 0,
                metrics: SimulationMetrics::default(),
                tick_times: VecDeque::new(),
                needed: HashSet::new(),
                physics_needed: HashSet::new(),
                physics,
                terrain: self.terrain.lock().take().expect("plugin built once"),
                scatter,
                spawn,
                spawn_chunks,
                actors: BTreeMap::new(),
                next_actor: 1,
                events: VecDeque::new(),
            })
            .add_systems(Update, advance);
        // The training dummy shares the player spawn volume so it is always
        // reachable on foot; `available_spawn` keeps it off occupied cells.
        // A titan hunts from further out so it does not immediately engage
        // whoever spawns.
        let mut kinds = vec![ActorKind::dummy()];
        if self.config.spawn_titan {
            kinds.push(ActorKind::titan());
        }
        // Offsets stay inside the eagerly generated spawn chunks (±1 chunk
        // around spawn ≈ 32 blocks) so `available_spawn` never sees void.
        let offsets = [
            spawn + Vec3::new(4.0, 0.0, 0.0),
            spawn + Vec3::new(24.0, 0.0, 0.0),
        ];
        let positions: Vec<Option<Vec3>> = {
            let world = app.world().resource::<VoxelWorld>();
            offsets
                .into_iter()
                .map(|offset| terrain_stream::available_spawn(world, offset, &[]))
                .collect()
        };
        {
            let mut sim = app.world_mut().resource_mut::<Simulation>();
            for (kind, position) in kinds.into_iter().zip(positions) {
                if let Some(position) = position {
                    sim.spawn_actor_kind(kind, position);
                }
            }
        }
    }
}
struct Player {
    state: PlayerState,
    health: Health,
    life: u64,
    respawn_requested: bool,
    last_input: u64,
    input: PlayerInput,
    pending: BTreeMap<u64, PlayerInput>,
    known: HashMap<IVec3, u64>,
    interest: HashSet<IVec3>,
    safety: HashSet<IVec3>,
    interest_center: Option<IVec3>,
    terrain_revision: u64,
    last_edit: u64,
    results: VecDeque<(u64, EditOutcome)>,
    highest_request: u64,
    physics_revision: Option<u64>,
    body_push_velocity: Vec3,
    // Deadline in 1/(60 * bow shots per second) seconds for fractional-tick cadence.
    next_bow_time: u64,
    arrow_revision: Option<u64>,
    package_revision: Option<u64>,
    /// Fixed tick when the next melee swing is allowed.
    attack_ready: u64,
    inventory: Inventory,
    inventory_dirty: bool,
    /// Requested package assets waiting for paced transmission.
    asset_queue: VecDeque<AssetDownload>,
}

/// One package file a player asked for; `bytes` caches the read so a file that
/// is edited mid-transfer still completes coherently.
struct AssetDownload {
    package: String,
    path: String,
    bytes: std::sync::Arc<Vec<u8>>,
    offset: u32,
}
impl Player {
    fn new() -> Self {
        Self {
            state: PlayerState::default(),
            health: Health::default(),
            life: 0,
            respawn_requested: false,
            last_input: 0,
            input: PlayerInput::default(),
            pending: BTreeMap::new(),
            known: HashMap::new(),
            interest: HashSet::new(),
            safety: HashSet::new(),
            interest_center: None,
            terrain_revision: 0,
            last_edit: 0,
            results: VecDeque::new(),
            highest_request: 0,
            physics_revision: None,
            body_push_velocity: Vec3::ZERO,
            next_bow_time: 0,
            arrow_revision: None,
            package_revision: None,
            inventory: Inventory::default(),
            attack_ready: 0,
            inventory_dirty: false,
            asset_queue: VecDeque::new(),
        }
    }
    fn snapshot(&self, id: u64) -> PlayerSnapshot {
        PlayerSnapshot {
            id,
            last_input: self.last_input,
            state: self.state,
            health: self.health,
            life: self.life,
            yaw: self.input.yaw,
        }
    }
    fn enqueue(&mut self, input: PlayerInput) {
        if input.sequence <= self.last_input
            || input.sequence > self.last_input.saturating_add(256)
            || self.pending.contains_key(&input.sequence)
        {
            return;
        }
        if self.health.is_depleted() {
            // Acknowledge discarded input without moving or retaining it. A client
            // may keep sending while dead; its sequence must not outrun admission.
            self.last_input = input.sequence;
            return;
        }
        if self.pending.len() == MAX_INPUT_QUEUE {
            return;
        }
        self.pending.insert(input.sequence, input);
    }
}
#[derive(Default)]
struct Journal {
    revision: u64,
    /// Post-edit voxel state per cell; restore must reproduce all three fields.
    voxels: BTreeMap<u16, Voxel>,
}
#[derive(Default, Debug)]
pub struct SimulationMetrics {
    pub generated: u64,
    pub accepted_edits: u64,
    pub rejected_edits: u64,
    pub disconnected: u64,
    pub snapshot_errors: u64,
    pub destroyed_blocks: u64,
    pub rejected_contacts: u64,
    pub bow_shots: u64,
    pub explosions: u64,
}
#[derive(Resource)]
pub struct Simulation {
    config: ServerConfig,
    transport: Option<ServerTransport>,
    players: HashMap<u64, Player>,
    journal: HashMap<IVec3, Journal>,
    journal_blocks: usize,
    last_needed: HashMap<IVec3, u64>,
    pub tick: u64,
    pub metrics: SimulationMetrics,
    tick_times: VecDeque<u64>,
    needed: HashSet<IVec3>,
    // Reused per tick so body-footprint residency keeps its capacity.
    physics_needed: HashSet<IVec3>,
    physics: Option<PhysicsSlice>,
    /// Sparse exceptions survive chunk eviction and transfer across loose/grid ownership.
    damage: HashMap<IVec3, DamageState>,
    /// Targets whose [`DamageState::release`] is set, keyed `(y, z, x)` so
    /// iteration matches the historical `(y, z, x)` release order without
    /// scanning and sorting the whole damage map every tick.
    release_queue: BTreeSet<(i32, i32, i32)>,
    strikes: VecDeque<QueuedStrike>,
    arrows: Vec<bow::Arrow>,
    detonations: VecDeque<(u32, Vec3, game_packages::BlastSpec, Option<SimEntity>)>,
    next_arrow: u32,
    arrow_revision: u64,
    drops: VecDeque<items::Drop>,
    next_drop: u32,
    drop_revision: u64,
    sent_drop_revision: u64,
    packages: game_packages::PackageHost,
    terrain: terrain_stream::TerrainStream,
    /// Scatter instances per resident chunk, sent once alongside the chunk.
    scatter: HashMap<IVec3, Vec<protocol::ScatterInstance>>,
    spawn: Vec3,
    spawn_chunks: HashSet<IVec3>,
    actors: actors::ActorMap,
    next_actor: u32,
    /// Backlog of per-tick attribution events for `drain_events`; bounded
    /// oldest-first so an undraining consumer cannot grow memory.
    events: VecDeque<SimEvent>,
}
impl Simulation {
    pub fn player_count(&self) -> usize {
        self.players.len()
    }
    /// Chunks a player is currently interested in; empty for unknown ids.
    /// Smoke tests assert client residency stays inside this set.
    pub fn player_interest(&self, id: u64) -> Option<&HashSet<IVec3>> {
        self.players.get(&id).map(|p| &p.interest)
    }
    pub fn print_metrics(&self, loaded_chunks: usize) {
        let mut times: Vec<_> = self.tick_times.iter().copied().collect();
        times.sort_unstable();
        let percentile = |percent: usize| {
            times
                .get(times.len().saturating_sub(1) * percent / 100)
                .copied()
                .unwrap_or(0)
        };
        eprintln!(
            "metrics tick={} players={} loaded_chunks={} generated={} journal_blocks={} accepted_edits={} rejected_edits={} rx_bytes={} tx_bytes={} udp_rejected={} tick_us_p50={} tick_us_p95={} tick_us_p99={} tick_us_max={}",
            self.tick,
            self.players.len(),
            loaded_chunks,
            self.metrics.generated,
            self.journal_blocks,
            self.metrics.accepted_edits,
            self.metrics.rejected_edits,
            self.transport
                .as_ref()
                .map_or(0, |net| net.stats.received_bytes),
            self.transport
                .as_ref()
                .map_or(0, |net| net.stats.sent_bytes),
            self.transport
                .as_ref()
                .map_or(0, |net| net.stats.rejected_datagrams),
            percentile(50),
            percentile(95),
            percentile(99),
            percentile(100)
        );
        eprintln!(
            "damage active={} destroyed={} rejected_contacts={} queued_strikes={}",
            self.damage.len(),
            self.metrics.destroyed_blocks,
            self.metrics.rejected_contacts,
            self.strikes.len()
        );
        eprintln!(
            "bow shots={} explosions={} flying={} pending_blasts={}",
            self.metrics.bow_shots,
            self.metrics.explosions,
            self.arrows.len(),
            self.detonations.len()
        );
        if let Some(p) = &self.physics {
            eprintln!(
                "physics submissions={} busy_periods={} readback_bytes={} terrain_upload_bytes={} completions={} observed_completion_us_mean={} observed_completion_us_max={} player_upload_bytes={} contact_overflow={}",
                p.submissions,
                p.busy_ticks,
                p.readback_bytes,
                p.terrain_upload_bytes,
                p.completions,
                p.completion_us_total / p.completions.max(1),
                p.completion_us_max,
                p.player_upload_bytes,
                p.contact_overflow()
            );
            eprintln!(
                "physics resident_chunks={} terrain_wait_periods={}",
                p.resident_chunks(),
                p.terrain_wait_ticks
            );
        }
    }
    fn drop_player(&mut self, id: u64) {
        if let Some(net) = &mut self.transport {
            net.disconnect(id);
        }
        self.strikes.retain(|strike| strike.id != id);
        if self.players.remove(&id).is_some() {
            self.metrics.disconnected += 1;
            eprintln!("disconnect id={id}");
        }
    }
    fn send(&mut self, id: u64, message: &ServerMessage) -> bool {
        if self
            .transport
            .as_mut()
            .is_some_and(|net| net.send(id, message).is_err())
        {
            self.drop_player(id);
            false
        } else {
            true
        }
    }
    /// Queue one input packet for a player. The packet's life token must match the
    /// player's current spawn generation; stale tokens are dropped, so movement
    /// from before a respawn cannot replay.
    fn accept_inputs(&mut self, id: u64, packet: InputPacket) {
        let Some(player) = self.players.get_mut(&id) else {
            return;
        };
        if packet.life != player.life {
            return;
        }
        for input in packet.inputs {
            player.enqueue(input);
        }
    }
    fn finish_edit(&mut self, id: u64, request: u64, outcome: EditOutcome) {
        if let Some(player) = self.players.get_mut(&id) {
            player.highest_request = player.highest_request.max(request);
            if outcome.is_ok() {
                player.last_edit = self.tick;
                self.metrics.accepted_edits += 1;
            } else {
                self.metrics.rejected_edits += 1;
            }
            player.results.push_back((request, outcome));
            if player.results.len() > 64 {
                player.results.pop_front();
            }
        }
        self.send_edit_result(id, request, outcome);
    }

    fn send_edit_result(&mut self, id: u64, request: u64, outcome: EditOutcome) {
        let accepted = outcome.is_ok();
        let reason = outcome.err();
        let damage = outcome.ok().flatten();
        eprintln!(
            "edit id={id} request={request} accepted={accepted} reason={reason:?} damage={damage:?}"
        );
        self.send(
            id,
            &ServerMessage::EditResult {
                request,
                accepted,
                reason,
                damage,
            },
        );
    }

    fn has_journal_space(&self, target: IVec3) -> bool {
        self.journal_blocks < MAX_JOURNAL_BLOCKS
            || self
                .journal
                .get(&chunk_coord(target))
                .is_some_and(|j| j.voxels.contains_key(&(index(local_coord(target)) as u16)))
    }

    /// Install one asynchronously generated chunk without clobbering live edits.
    fn install_generated(
        &mut self,
        world: &mut VoxelWorld,
        coord: IVec3,
        chunk: Chunk,
        scatter: Vec<voxel_world::terrain::ScatterInstance>,
    ) {
        // Ignore stale completions and never replace a chunk edited while work was pending.
        if self.needed.contains(&coord) && !world.chunks.contains_key(&coord) {
            world.insert(coord, chunk);
            restore(world, coord, self.journal.get(&coord));
            self.scatter.insert(coord, wire_scatter(scatter));
            self.metrics.generated += 1;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn edit(
        &mut self,
        world: &mut VoxelWorld,
        id: u64,
        request: u64,
        target: IVec3,
        block: u8,
        expected_revision: u64,
        strike: bool,
    ) {
        let Some(player) = self.players.get(&id) else {
            return;
        };
        if let Some((_, outcome)) = player.results.iter().find(|(old, _)| *old == request) {
            self.send_edit_result(id, request, *outcome);
            return;
        }
        if self
            .strikes
            .iter()
            .any(|s| s.id == id && s.request == request)
        {
            return;
        }
        if player.health.is_depleted() {
            self.finish_edit(id, request, Err(EditRejection::Dead));
            return;
        }
        let validation = validate_player_edit(
            world,
            player,
            self.tick,
            request,
            target,
            block,
            expected_revision,
        );
        if let Err(reason) = validation {
            self.finish_edit(id, request, Err(reason));
            return;
        }
        // Strikes dig smooth terrain without GPU physics, but a queued strike
        // still waits for an in-flight batch so its revision check stays honest.
        if strike && self.physics.as_ref().is_some_and(|p| p.is_busy()) {
            if self.strikes.iter().filter(|s| s.id == id).count() >= MAX_QUEUED_STRIKES {
                self.finish_edit(id, request, Err(EditRejection::QueueFull));
            } else {
                self.strikes.push_back(QueuedStrike {
                    id,
                    request,
                    target,
                    revision: expected_revision,
                    expires: self.tick.saturating_add(STRIKE_QUEUE_TICKS),
                });
                self.players.get_mut(&id).unwrap().highest_request = request;
            }
            return;
        }
        let outcome = self.execute_edit(world, id, target, block, strike);
        self.finish_edit(id, request, outcome);
    }

    fn drain_strikes(&mut self, world: &mut VoxelWorld) {
        let strikes = std::mem::take(&mut self.strikes);
        for strike in strikes {
            let Some(player) = self.players.get(&strike.id) else {
                continue;
            };
            if player.health.is_depleted() {
                self.finish_edit(strike.id, strike.request, Err(EditRejection::Dead));
                continue;
            }
            let validation = if self.tick >= strike.expires {
                Err(EditRejection::Expired)
            } else {
                validate_edit_target(world, player, strike.target, 0, strike.revision)
            };
            if let Err(reason) = validation {
                self.finish_edit(strike.id, strike.request, Err(reason));
                continue;
            }
            if self.physics.as_ref().is_some_and(|p| p.is_busy())
                || self.tick < player.last_edit.saturating_add(6)
            {
                self.strikes.push_back(strike);
                continue;
            }
            let outcome = self.execute_edit(world, strike.id, strike.target, 0, true);
            self.finish_edit(strike.id, strike.request, outcome);
        }
    }

    fn execute_edit(
        &mut self,
        world: &mut VoxelWorld,
        id: u64,
        target: IVec3,
        block: u8,
        strike: bool,
    ) -> EditOutcome {
        if block == 0 && !strike {
            let material = world.block(target).ok_or(EditRejection::InvalidTarget)?;
            return self
                .apply_contact(world, &tool_contact(target, material))
                .map(Some);
        }
        if block != 0 {
            return Err(EditRejection::InvalidTarget);
        }
        if !self.has_journal_space(target) {
            return Err(EditRejection::StorageFull);
        }
        // Re-raycast the validated aim: the dig sphere centers just inside
        // the struck surface, not on the cell the client named.
        let player = &self.players[&id];
        let eye = player.state.position + Vec3::Y * EYE_HEIGHT;
        let direction = look_direction(player.input.yaw, player.input.pitch);
        let hit = world
            .raycast(eye, direction, 6.0)
            .filter(|hit| hit.block == target)
            .ok_or(EditRejection::OutOfReach)?;
        let center = eye + direction * (hit.distance + STRIKE_DIG_DEPTH);
        let edits = world.brush_dig(center, STRIKE_DIG_RADIUS);
        if edits.is_empty() {
            return Err(EditRejection::InvalidTarget);
        }
        // One unit of the dominant destroyed material, matching the old
        // single-block yield.
        if let Some(material) = self.record_brush(edits) {
            self.destroyed(target, material, None);
        }
        Ok(None)
    }

    fn apply_contact(
        &mut self,
        world: &mut VoxelWorld,
        contact: &gpu_physics::TerrainContact,
    ) -> Result<f32, EditRejection> {
        let target = IVec3::from_array(contact.target);
        let journal_space = self.has_journal_space(target);
        let result =
            material_damage::apply_to_grid(world, &mut self.damage, contact, journal_space)?;
        // Keep the release work set in step with the damage map: `release` is
        // latched (never cleared while the entry lives), so this is a simple
        // insert/remove rather than a per-tick rescan.
        if result.release {
            self.release_queue.insert(release_key(target));
        } else {
            self.release_queue.remove(&release_key(target));
        }
        if let Some(edits) = result.destroyed_edits {
            for edit in &edits {
                self.record_voxels(edit);
            }
            self.destroyed(target, contact.material as u8, None);
        }
        Ok(result.fraction)
    }

    /// Common destruction event for grid and loose ownership. Material identity is
    /// all a drop needs today; a richer item registry can intercept here later.
    fn destroyed(&mut self, target: IVec3, material: u8, body: Option<u32>) {
        self.metrics.destroyed_blocks += 1;
        eprintln!("destroyed target={target} material={material} body={body:?}");
        self.spawn_drop(target.as_vec3() + Vec3::splat(0.5), u32::from(material), 1);
    }
    /// One authoritative voxel transaction path for edits, detachment, and settlement.
    fn record_change(&mut self, _world: &VoxelWorld, target: IVec3, voxel: Voxel, from: u64, to: u64) {
        let coord = chunk_coord(target);
        let cell = index(local_coord(target)) as u16;
        self.record_chunk_edit(coord, from, to, &[(cell, voxel)]);
    }

    /// Journal and broadcast one brush edit's per-chunk voxel list.
    fn record_voxels(&mut self, edit: &BrushEdit) {
        let voxels: Vec<(u16, Voxel)> = edit
            .voxels
            .iter()
            .map(|&(cell, _, after)| (cell, after))
            .collect();
        self.record_chunk_edit(edit.coord, edit.from, edit.to, &voxels);
    }

    /// Record a brush's edits. Returns the dominant destroyed material;
    /// ties resolve to the lowest id.
    fn record_brush(&mut self, edits: Vec<BrushEdit>) -> Option<u8> {
        let mut counts = BTreeMap::new();
        for edit in &edits {
            for &(_, before, after) in &edit.voxels {
                if before.material != voxel_world::AIR && after.material == voxel_world::AIR {
                    *counts.entry(before.material).or_insert(0u32) += 1;
                }
            }
        }
        for edit in &edits {
            self.record_voxels(edit);
        }
        counts
            .iter()
            .max_by_key(|&(_, &count)| count)
            .map(|(&material, _)| material)
    }

    fn record_chunk_edit(&mut self, coord: IVec3, from: u64, to: u64, voxels: &[(u16, Voxel)]) {
        if !self.journal.contains_key(&coord) {
            // Other players must discover new construction beyond natural surfaces.
            for player in self.players.values_mut() {
                player.interest_center = None;
            }
        }
        let journal = self.journal.entry(coord).or_default();
        journal.revision = to;
        for &(cell, voxel) in voxels {
            if journal.voxels.insert(cell, voxel).is_none() {
                self.journal_blocks += 1;
            }
            let cell = i32::from(cell);
            let pos = coord * CHUNK_SIZE + IVec3::new(cell % 32, cell / 1024, (cell / 32) % 32);
            // Fracture progress is bound to material identity; a density-only
            // change keeps it, a material change clears it.
            if self
                .damage
                .get(&pos)
                .is_some_and(|state| state.material != voxel.material)
            {
                self.damage.remove(&pos);
                self.release_queue.remove(&release_key(pos));
            }
            if let Some(physics) = &mut self.physics {
                physics.set_voxel(pos, voxel.material);
            }
        }
        let wire: Vec<(u16, protocol::Voxel)> = voxels
            .iter()
            .map(|&(cell, voxel)| {
                (
                    cell,
                    protocol::Voxel {
                        material: voxel.material,
                        density: voxel.density,
                        placed: voxel.placed,
                    },
                )
            })
            .collect();
        let recipients: Vec<_> = self
            .players
            .iter()
            .filter(|(_, p)| p.known.get(&coord) == Some(&from))
            .map(|(&id, _)| id)
            .collect();
        for recipient in recipients {
            if self.send(
                recipient,
                &ServerMessage::Delta {
                    coord,
                    from,
                    to,
                    voxels: wire.clone(),
                },
            ) {
                self.players
                    .get_mut(&recipient)
                    .unwrap()
                    .known
                    .insert(coord, to);
            }
        }
    }

    /// Finish observed contacts before edits can transfer voxel ownership.
    fn observe_physics(&mut self, world: &mut VoxelWorld) {
        let Some(mut physics) = self.physics.take() else {
            return;
        };
        physics.poll();
        for (id, target, material) in physics.destroyed_bodies() {
            physics.remove(id);
            self.destroyed(target, material, Some(id));
        }
        let contacts = physics.take_terrain_contacts(world);
        self.physics = Some(physics);
        for contact in contacts {
            if self.apply_contact(world, &contact).is_err() {
                self.metrics.rejected_contacts += 1;
            }
        }
    }

    fn advance_physics(&mut self, world: &mut VoxelWorld) {
        let Some(mut physics) = self.physics.take() else {
            return;
        };
        // Attachment failure is latched, not a sum of old contact loads. Retry the
        // ownership transaction when body capacity/readback permits it. The work
        // set holds only latched targets, in the historical (y, z, x) order, so
        // no per-tick scan/sort of the whole damage map is needed.
        let releases: Vec<(i32, i32, i32)> = self.release_queue.iter().copied().collect();
        for key in releases {
            let target = IVec3::new(key.2, key.0, key.1);
            let Some(state) = self.damage.get(&target).copied() else {
                self.release_queue.remove(&key);
                continue;
            };
            if !state.release {
                self.release_queue.remove(&key);
                continue;
            }
            if world.block(target) == Some(state.material)
                && physics.can_detach(target)
                && self.has_journal_space(target)
                && let Some((from, to)) = world.set_block(target, 0)
            {
                // The solver already reacted the collision impulse into the grid.
                physics.release(target, state.material, state.joules, Vec3::ZERO);
                physics.set_voxel(target, 0);
                self.record_change(world, target, Voxel::AIR, from, to);
                self.release_queue.remove(&key);
            }
        }
        // Stable body IDs establish deterministic order for competing cell claims.
        for (id, target, material) in physics.settling_candidates() {
            for target in physics.placement_cells(id, target) {
                let coord = chunk_coord(target);
                let has_journal_space = self.journal_blocks < MAX_JOURNAL_BLOCKS
                    || self.journal.get(&coord).is_some_and(|j| {
                        j.voxels.contains_key(&(index(local_coord(target)) as u16))
                    });
                if !(has_journal_space
                    && (if physics.failed() {
                        world.block(target) == Some(0)
                            && !self
                                .players
                                .values()
                                .any(|p| overlaps_block(&p.state, target))
                    } else {
                        can_settle(world, target, self.players.values().map(|p| &p.state))
                    })
                    && !physics.overlaps_except(target, id)
                    && (self.damage.len() < MAX_DAMAGED_BLOCKS
                        || self.damage.contains_key(&target)
                        || physics.body_damage(id) == 0.0))
                {
                    continue;
                }
                // Failed physics restores at the source cell; a live body
                // deposits where it came to rest.
                let center = if physics.failed() {
                    target.as_vec3() + Vec3::splat(0.5)
                } else {
                    physics.body_position(id).unwrap_or(target.as_vec3() + Vec3::splat(0.5))
                };
                let edits = world.brush_add(center, SETTLE_RADIUS, material);
                if edits.is_empty() {
                    continue;
                }
                let damage = physics.body_damage(id);
                physics.remove(id);
                for edit in &edits {
                    for &(cell, _, _) in &edit.voxels {
                        let cell = i32::from(cell);
                        physics.set_voxel(
                            edit.coord * CHUNK_SIZE
                                + IVec3::new(cell % 32, cell / 1024, (cell / 32) % 32),
                            material,
                        );
                    }
                    self.record_voxels(edit);
                }
                if damage > 0.0 {
                    self.damage.insert(
                        target,
                        DamageState {
                            material,
                            joules: damage,
                            release: false,
                        },
                    );
                    self.release_queue.remove(&release_key(target));
                }
                break;
            }
        }
        // Collider prep is sorted and collected only when the upcoming step can
        // actually consume it; off-cadence and busy ticks would discard the work.
        if physics.can_prepare_submission() {
            let mut players: Vec<_> = self.players.iter().collect();
            players.sort_unstable_by_key(|(id, _)| **id);
            physics.set_player_colliders(
                players
                    .into_iter()
                    .filter(|(_, player)| !player.state.noclip && !player.health.is_depleted())
                    .enumerate()
                    .map(|(slot, (_, player))| gpu_physics::PlayerCollider {
                        position: player.state.position.to_array(),
                        id: slot as u32,
                        velocity: player.body_push_velocity.to_array(),
                        padding: 0,
                    }),
            );
        }
        physics.step(world);
        if self.tick.is_multiple_of(3) {
            let revision = physics.revision;
            let ids: Vec<_> = self
                .players
                .iter()
                .filter(|(_, player)| player.physics_revision != Some(revision))
                .map(|(&id, _)| id)
                .collect();
            if !ids.is_empty() {
                let message = ServerMessage::Physics {
                    tick: revision,
                    bodies: physics.snapshots(),
                };
                for id in ids {
                    if self.send(id, &message) {
                        self.players.get_mut(&id).unwrap().physics_revision = Some(revision);
                    }
                }
            }
        }
        self.physics = Some(physics);
    }
    /// Take the buffered event stream, leaving an empty backlog.
    /// Events persist until drained (never cleared per tick); when the backlog
    /// reaches `MAX_QUEUED_EVENTS` the oldest events drop so the most recent
    /// history is always retained for a consumer that drains late or never.
    pub fn drain_events(&mut self) -> Vec<SimEvent> {
        std::mem::take(&mut self.events).into()
    }
}
fn validate_player_edit(
    world: &VoxelWorld,
    player: &Player,
    tick: u64,
    request: u64,
    target: IVec3,
    block: u8,
    revision: u64,
) -> Result<(), EditRejection> {
    if request <= player.highest_request {
        return Err(EditRejection::OldRequest);
    }
    if tick < player.last_edit.saturating_add(6) {
        return Err(EditRejection::Cooldown);
    }
    validate_edit_target(world, player, target, block, revision)
}

fn validate_edit_target(
    world: &VoxelWorld,
    player: &Player,
    target: IVec3,
    block: u8,
    revision: u64,
) -> Result<(), EditRejection> {
    let coord = chunk_coord(target);
    if block > voxel_world::WOOD
        || target.y <= MIN_CHUNK_Y * CHUNK_SIZE
        || !valid_coord(coord)
        || !player.interest.contains(&coord)
    {
        return Err(EditRejection::InvalidTarget);
    }
    if player.known.get(&coord) != Some(&revision)
        || world
            .chunks
            .get(&coord)
            .is_none_or(|c| c.revision != revision)
    {
        return Err(EditRejection::StaleRevision);
    }
    if !world.block(target).is_some_and(|existing| {
        if block == 0 {
            existing != 0
        } else {
            existing == 0
        }
    }) {
        return Err(EditRejection::InvalidTarget);
    }
    if !world
        .raycast(
            player.state.position + Vec3::Y * EYE_HEIGHT,
            look_direction(player.input.yaw, player.input.pitch),
            6.0,
        )
        .is_some_and(|hit| {
            if block == 0 {
                hit.block == target
            } else {
                hit.adjacent == target
            }
        })
    {
        return Err(EditRejection::OutOfReach);
    }
    Ok(())
}

#[cfg(test)]
fn valid_player_edit(
    world: &VoxelWorld,
    player: &Player,
    tick: u64,
    request: u64,
    target: IVec3,
    block: u8,
    revision: u64,
) -> bool {
    validate_player_edit(world, player, tick, request, target, block, revision).is_ok()
}
fn can_settle<'a>(
    world: &VoxelWorld,
    target: IVec3,
    players: impl Iterator<Item = &'a PlayerState>,
) -> bool {
    world.block(target) == Some(0)
        && world.block(target - IVec3::Y).is_some_and(|b| b != 0)
        && !players.into_iter().any(|p| overlaps_block(p, target))
}
fn valid_coord(coord: IVec3) -> bool {
    coord.x.abs_diff(0) <= 1_000_000
        && coord.z.abs_diff(0) <= 1_000_000
        && (MIN_CHUNK_Y..=MAX_CHUNK_Y).contains(&coord.y)
}
/// Convert compiled scatter instances to their wire form.
fn wire_scatter(
    instances: Vec<voxel_world::terrain::ScatterInstance>,
) -> Vec<protocol::ScatterInstance> {
    instances
        .into_iter()
        .map(|instance| protocol::ScatterInstance {
            species: instance.species,
            x: instance.x,
            y: instance.y,
            z: instance.z,
            yaw: instance.yaw,
            scale: instance.scale,
        })
        .collect()
}

fn restore(world: &mut VoxelWorld, coord: IVec3, journal: Option<&Journal>) {
    world.ensure_chunk(coord);
    if let Some(journal) = journal {
        for (&cell, &voxel) in &journal.voxels {
            let cell = cell as i32;
            let local = IVec3::new(cell % 32, cell / 1024, (cell / 32) % 32);
            world.set_voxel(coord * CHUNK_SIZE + local, voxel);
        }
        let (material_runs, density_runs) = world.chunks[&coord].cached_voxel_runs();
        world.insert(
            coord,
            Chunk::from_voxel_runs(journal.revision, &material_runs, &density_runs)
                .expect("journal contains valid voxels"),
        );
    }
}
fn advance(mut simulation: ResMut<Simulation>, mut world: ResMut<VoxelWorld>) {
    let started = Instant::now();
    let sim = &mut *simulation;
    sim.tick += 1;
    sim.poll_packages();
    for (coord, chunk, scatter) in sim.terrain.poll() {
        sim.install_generated(&mut world, coord, chunk, scatter);
    }
    sim.observe_physics(&mut world);
    let incoming = match sim.transport.as_mut().map_or_else(
        || Ok(networking::ServerIncoming::default()),
        |net| net.poll(),
    ) {
        Ok(incoming) => incoming,
        Err(error) => {
            eprintln!("transport poll failed: {error}");
            return;
        }
    };
    for id in incoming.disconnected {
        sim.drop_player(id);
    }
    for (id, session) in incoming.connected {
        let mut player = Player::new();
        let bodies = sim
            .physics
            .as_ref()
            .map(|p| p.dynamic_colliders())
            .unwrap_or_default();
        let Some(position) = terrain_stream::available_spawn(&world, sim.spawn, &bodies) else {
            eprintln!("connect id={id} rejected: no clear supported spawn in the loaded area");
            sim.drop_player(id);
            continue;
        };
        player.state.position = position;
        let melee = sim.packages.melee_table();
        for &(item, count) in melee.spawn_items() {
            if melee.kind(item) == protocol::ItemKind::Equipment {
                for _ in 0..count {
                    player.inventory.add_equipment(item);
                }
            } else {
                player.inventory.add(item, count);
            }
        }
        let spawn = player.state;
        let health = player.health;
        let inventory = player.inventory.clone();
        sim.players.insert(id, player);
        let scatter_species = (0..world.generator.species_count())
            .map(|index| protocol::ScatterSpeciesInfo {
                name: world.generator.species_name(index as u16).to_string(),
                package: game_packages::TERRAIN_PACKAGE.to_string(),
                model: world.generator.species_model(index as u16).to_string(),
            })
            .collect();
        sim.send(
            id,
            &ServerMessage::Welcome {
                id,
                session,
                seed: sim.config.seed,
                spawn,
                health,
                inventory,
                scatter_species,
            },
        );
        eprintln!("connect id={id}");
    }
    for (id, packet) in incoming.inputs {
        sim.accept_inputs(id, packet);
    }
    // Only one command advances each actor per server tick, regardless of packet rate.
    let bodies = sim
        .physics
        .as_ref()
        .map(|p| p.dynamic_colliders())
        .unwrap_or_default();
    for player in sim.players.values_mut() {
        if player.health.is_depleted() {
            // Dead players are frozen: no input movement, noclip, or GPU body push
            // until an explicit respawn restores a safe supported position.
            player.input = PlayerInput {
                movement: [0.0; 2],
                jump: false,
                descend: false,
                attack: false,
                ..player.input
            };
            player.state.velocity = Vec3::ZERO;
            player.state.external_velocity = glam::Vec2::ZERO;
            player.body_push_velocity = Vec3::ZERO;
        } else {
            let input = if let Some((sequence, input)) = player.pending.pop_first() {
                player.last_input = sequence;
                player.input = input;
                input
            } else {
                PlayerInput {
                    movement: [0.0; 2],
                    jump: false,
                    descend: false,
                    ..player.input
                }
            };
            let previous = player.state;
            // The same pass reports the horizontal velocity attempted before
            // loose bodies clamped the sweep; the GPU resolves that kinematic
            // push against material mass and terrain.
            let attempted = controller::step_player_with_bodies(
                &world,
                &mut player.state,
                &input,
                FIXED_DT,
                &bodies,
            );
            player.body_push_velocity =
                Vec3::new(attempted.x, player.state.velocity.y, attempted.y);
            if player.state.noclip {
                player.body_push_velocity = Vec3::ZERO;
            }
            if !player.state.position.is_finite()
                || player.state.position.x.abs() >= 31_999_900.0
                || player.state.position.z.abs() >= 31_999_900.0
            {
                player.state = previous;
                player.state.velocity = Vec3::ZERO;
            }
        }
        let center = chunk_coord(player.state.position.floor().as_ivec3());
        if player.interest_center != Some(center) || player.terrain_revision != sim.terrain.revision
        {
            player.interest = interests(
                player.state.position,
                sim.config.radius,
                &sim.terrain.bounds,
                sim.journal.keys().copied(),
            );
            player.safety = interests(
                player.state.position,
                sim.config.radius + 1,
                &sim.terrain.bounds,
                sim.journal.keys().copied(),
            );
            player
                .safety
                .extend(terrain_stream::local_chunks(player.state.position, 3));
            player.interest_center = Some(center);
            player.terrain_revision = sim.terrain.revision;
        }
    }
    sim.advance_actors(&world, &bodies);
    sim.resolve_attacks(&world);
    for (id, message) in incoming.reliable {
        match message {
            ClientMessage::Edit {
                request,
                target,
                block,
                expected_revision,
            } => sim.edit(
                &mut world,
                id,
                request,
                target,
                block,
                expected_revision,
                false,
            ),
            ClientMessage::Strike {
                request,
                target,
                expected_revision,
            } => sim.edit(&mut world, id, request, target, 0, expected_revision, true),
            ClientMessage::FireBow {
                request,
                yaw,
                pitch,
                power,
            } => sim.fire_bow(&world, id, request, yaw, pitch, power),
            ClientMessage::Resync { coord } => {
                if let Some(player) = sim.players.get_mut(&id) {
                    // Baseline transmission has a bounded per-tick batch. Retain
                    // every requested repair: dropping a burst here can strand a client
                    // on a stale revision forever.
                    if player.interest.contains(&coord) {
                        player.known.remove(&coord);
                    }
                }
            }
            ClientMessage::Respawn { life } => {
                sim.request_respawn(id, life);
            }
            ClientMessage::AssetRequest { package, path } => {
                sim.request_asset(id, &package, &path);
            }
            ClientMessage::Hello { .. } => {}
        }
    }
    sim.finish_respawns(&world);
    sim.drain_strikes(&mut world);
    sim.advance_bow(&mut world);
    sim.replicate_packages();
    sim.advance_physics(&mut world);
    sim.advance_drops(&world);
    sim.collect_drops();
    sim.replicate_inventory();
    sim.advance_assets();
    if sim.tick.is_multiple_of(3) {
        sim.replicate_drops();
    }
    let mut needed = std::mem::take(&mut sim.needed);
    // One chunk of safety beyond visible interest ensures swept bodies never reach unloaded edges.
    for player in sim.players.values() {
        needed.extend(player.safety.iter().copied());
    }
    // Body collision residency is independent of visible/player interest.
    sim.physics_needed.clear();
    if let Some(physics) = sim.physics.as_ref() {
        physics.needed_chunks(&mut sim.physics_needed);
    }
    needed.extend(sim.physics_needed.iter().copied());
    // Keep spawn warm for reconnects without admitting actors over unloaded terrain.
    needed.extend(sim.spawn_chunks.iter().copied());
    for &coord in &needed {
        sim.last_needed.insert(coord, sim.tick);
    }
    let missing: Vec<_> = needed
        .iter()
        .copied()
        .filter(|coord| !world.chunks.contains_key(coord) && !sim.terrain.generating(coord))
        .collect();
    let missing = streaming::nearest_chunks(missing, streaming::CHUNKS_PER_TICK, |coord| {
        (
            !sim.physics_needed.contains(coord),
            sim.players
                .values()
                .map(|p| {
                    let delta = *coord - chunk_coord(p.state.position.floor().as_ivec3());
                    delta.as_vec3().length_squared() as u64
                })
                .min()
                .unwrap_or(0),
        )
    });
    for coord in missing {
        sim.terrain.generate(coord);
    }
    let centers: Vec<_> = sim
        .players
        .values()
        .map(|p| chunk_coord(p.state.position.floor().as_ivec3()))
        .collect();
    sim.terrain.survey(&centers, sim.config.radius + 2);
    let evicted: Vec<_> = world
        .chunks
        .keys()
        .copied()
        .filter(|coord| {
            !needed.contains(coord)
                && sim
                    .tick
                    .saturating_sub(*sim.last_needed.get(coord).unwrap_or(&0))
                    >= 120
        })
        .collect();
    for coord in evicted {
        world.remove(coord);
        sim.scatter.remove(&coord);
        sim.last_needed.remove(&coord);
    }
    // A player can abandon a desired chunk before its generation turn. Those
    // coordinates must not accumulate in bookkeeping after they leave interest.
    sim.last_needed
        .retain(|coord, _| needed.contains(coord) || world.chunks.contains_key(coord));
    sim.needed = needed;
    let ids: Vec<_> = sim.players.keys().copied().collect();
    for id in &ids {
        let Some(player) = sim.players.get(id) else {
            continue;
        };
        let forgotten: Vec<_> = player
            .known
            .keys()
            .copied()
            .filter(|coord| !player.interest.contains(coord))
            .collect();
        for coord in forgotten {
            if !sim.send(*id, &ServerMessage::Forget { coord }) {
                break;
            }
            sim.players.get_mut(id).unwrap().known.remove(&coord);
        }
        let Some(player) = sim.players.get(id) else {
            continue;
        };
        let center = chunk_coord(player.state.position.floor().as_ivec3());
        let available: Vec<_> = player
            .interest
            .iter()
            .copied()
            .filter(|coord| !player.known.contains_key(coord) && world.chunks.contains_key(coord))
            .collect();
        let available = streaming::nearest_chunks(available, streaming::CHUNKS_PER_TICK, |coord| {
            (*coord - center).as_i64vec3().length_squared()
        });
        for coord in available {
            let chunk = &world.chunks[&coord];
            let (material_runs, density_runs) = chunk.cached_voxel_runs();
            let revision = chunk.revision;
            // Every resident chunk should already carry an entry; compute on
            // demand so a future eager install path cannot silently drop it.
            let scatter = match sim.scatter.get(&coord) {
                Some(instances) => instances.clone(),
                None => {
                    let instances =
                        wire_scatter(world.generator.scatter_chunk(coord, sim.config.seed));
                    sim.scatter.insert(coord, instances.clone());
                    instances
                }
            };
            if !sim.send(
                *id,
                &ServerMessage::Chunk {
                    coord,
                    revision,
                    material_runs,
                    density_runs,
                    scatter,
                },
            ) {
                break;
            }
            sim.players
                .get_mut(id)
                .unwrap()
                .known
                .insert(coord, chunk.revision);
        }
    }
    if sim.tick.is_multiple_of(3) {
        // Snapshot every entity once per tick, bucketed by the chunk its
        // position occupies; each recipient then gathers only the buckets its
        // interest set covers instead of rescanning all entities. The bucket
        // map costs one allocation per occupied chunk rather than a filtered
        // scan plus a Vec per recipient.
        let mut player_buckets: HashMap<IVec3, Vec<PlayerSnapshot>> = HashMap::new();
        for (&id, player) in &sim.players {
            let chunk = chunk_coord(player.state.position.floor().as_ivec3());
            player_buckets
                .entry(chunk)
                .or_default()
                .push(player.snapshot(id));
        }
        let actor_buckets = sim.actor_snapshot_buckets();
        // Scratch buffers are handed to each Snapshot, then reclaimed for the
        // next recipient, so capacity persists across the whole loop.
        let mut player_scratch: Vec<PlayerSnapshot> = Vec::new();
        let mut actor_scratch: Vec<ActorSnapshot> = Vec::new();
        for id in ids {
            let Some(player) = sim.players.get(&id) else {
                continue;
            };
            for coord in &player.interest {
                if let Some(bucket) = player_buckets.get(coord) {
                    player_scratch.extend(
                        bucket
                            .iter()
                            .filter(|s| s.id != id)
                            .cloned(),
                    );
                }
                if let Some(bucket) = actor_buckets.get(coord) {
                    actor_scratch.extend_from_slice(bucket);
                }
            }
            // Bucketing scrambles the actor map's id order; sort to keep the
            // emitted order identical to scanning the BTreeMap.
            actor_scratch.sort_by_key(|s| s.id);
            player_scratch.sort_by_key(|s| s.id);
            let mut snapshot = Snapshot {
                tick: sim.tick,
                you: player.snapshot(id),
                players: std::mem::take(&mut player_scratch),
                actors: std::mem::take(&mut actor_scratch),
            };
            if sim
                .transport
                .as_mut()
                .is_some_and(|net| net.snapshot(id, &snapshot).is_err())
            {
                sim.metrics.snapshot_errors += 1;
            }
            player_scratch = std::mem::take(&mut snapshot.players);
            actor_scratch = std::mem::take(&mut snapshot.actors);
            player_scratch.clear();
            actor_scratch.clear();
        }
    }
    sim.tick_times
        .push_back(started.elapsed().as_micros() as u64);
    if sim.tick_times.len() > 3600 {
        sim.tick_times.pop_front();
    }
    if sim.config.metrics_every > 0 && sim.tick.is_multiple_of(sim.config.metrics_every) {
        sim.print_metrics(world.chunks.len());
    }
}

fn observe_character_bodies(sim: Res<Simulation>, mut bodies: ResMut<controller::ObservedBodies>) {
    bodies.0 = sim
        .physics
        .as_ref()
        .map(|p| p.dynamic_colliders())
        .unwrap_or_default();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn eviction_restores_edit_and_revision_even_after_reverting_cell() {
        let coord = IVec3::ZERO;
        let target = IVec3::new(1, 20, 1);
        let mut world = VoxelWorld::default();
        world.ensure_chunk(coord);
        let original = world.voxel(target).unwrap();
        let changed = if original.material == 0 { 3 } else { 0 };
        world.set_block(target, changed).unwrap();
        let (_, revision) = world.set_block(target, original.material).unwrap();
        let restored = world.voxel(target).unwrap();
        let journal = Journal {
            revision,
            voxels: BTreeMap::from([(index(target) as u16, restored)]),
        };
        world.remove(coord);
        restore(&mut world, coord, Some(&journal));
        assert_eq!(world.voxel(target), Some(restored));
        assert_eq!(world.chunks[&coord].revision, 2);
    }

    fn empty_world() -> VoxelWorld {
        let mut world = VoxelWorld::default();
        world.chunks.insert(
            IVec3::ZERO,
            std::sync::Arc::new(Chunk::from_runs(0, &[(32768, 0)]).unwrap()),
        );
        world
    }

    #[test]
    fn settlement_requires_air_support_and_player_clearance() {
        let mut world = empty_world();
        let target = IVec3::new(2, 2, 2);
        assert!(!can_settle(&world, target, std::iter::empty()));
        world.set_block(target - IVec3::Y, 3).unwrap();
        assert!(can_settle(&world, target, std::iter::empty()));
        let player = PlayerState {
            position: target.as_vec3() + Vec3::new(0.5, 0.0, 0.5),
            ..Default::default()
        };
        assert!(!can_settle(&world, target, std::iter::once(&player)));
        world.set_block(target, 2).unwrap();
        assert!(!can_settle(&world, target, std::iter::empty()));
    }

    #[test]
    fn journal_tracks_detachment_and_regridding() {
        let mut world = empty_world();
        let source = IVec3::new(2, 2, 2);
        let destination = IVec3::new(3, 2, 2);
        world.set_block(source, 5).unwrap();
        world.set_block(source, 0).unwrap();
        let (_, to) = world.set_block(destination, 5).unwrap();
        let journal = Journal {
            revision: to,
            voxels: BTreeMap::from([
                (index(source) as u16, Voxel::AIR),
                (index(destination) as u16, Voxel::placed(5)),
            ]),
        };
        world.remove(IVec3::ZERO);
        restore(&mut world, IVec3::ZERO, Some(&journal));
        assert_eq!(world.voxel(source), Some(Voxel::AIR));
        assert_eq!(world.voxel(destination), Some(Voxel::placed(5)));
        assert_eq!(world.chunks[&IVec3::ZERO].revision, to);
    }

    #[test]
    fn edits_validate_revision_ray_request_and_cooldown() {
        let mut world = empty_world();
        let target = IVec3::new(2, 3, 2);
        world.set_block(target, 5).unwrap();
        let mut player = Player::new();
        player.state.position = Vec3::new(2.5, 2.0, 5.5);
        player.interest.insert(IVec3::ZERO);
        player.known.insert(IVec3::ZERO, 1);
        assert!(valid_player_edit(&world, &player, 100, 1, target, 0, 1));
        assert!(!valid_player_edit(&world, &player, 100, 1, target, 0, 0));
        assert!(!valid_player_edit(&world, &player, 100, 0, target, 0, 1));
        assert!(!valid_player_edit(&world, &player, 1, 1, target, 0, 1));
        world.set_block(target + IVec3::Z, 3).unwrap();
        player.known.insert(IVec3::ZERO, 2);
        assert!(!valid_player_edit(&world, &player, 100, 1, target, 0, 2));
        assert_eq!(world.block(target), Some(5));
    }

    #[test]
    fn melee_swings_damage_knockback_and_respawn_actors() {
        let mut app = headless_app(1);
        let (_generated, mut sim) = take_resources(&mut app);
        let mut world = empty_world();
        // Ground plane so the actor and player stand on terrain.
        for x in 0..8 {
            for z in 0..8 {
                world.set_block(IVec3::new(x, 0, z), voxel_world::STONE).unwrap();
            }
        }
        let dummy = sim.spawn_actor(Vec3::new(2.5, 1.0, 0.5)).unwrap();
        let mut player = Player::new();
        player.state.position = Vec3::new(2.5, 1.0, 3.5);
        player.input.pitch = 0.0;
        player.input.yaw = 0.0; // -Z faces the dummy
        player.input.attack = true;
        sim.players.insert(1, player);

        sim.tick = 100;
        sim.resolve_attacks(&world);
        // A level swing at eye height lands in the head zone: double damage.
        assert_eq!(
            sim.actor_health(dummy).unwrap().current(),
            100 - gameplay::combat::MELEE_HANDS.damage * gameplay::combat::HEADSHOT_MULTIPLIER
        );
        assert!(
            sim.actor_position(dummy).unwrap().z < 0.5
                || sim.actors[&dummy].state.motion.external_velocity.length() > 0.0,
            "knockback did not move the dummy"
        );
        // Cooldown blocks an immediate second swing.
        sim.resolve_attacks(&world);
        assert_eq!(
            sim.actor_health(dummy).unwrap().current(),
            100 - gameplay::combat::MELEE_HANDS.damage * gameplay::combat::HEADSHOT_MULTIPLIER
        );
        // Ten swings at cooldown spacing kill the dummy; it respawns later.
        for _ in 0..10 {
            sim.tick += u64::from(gameplay::combat::MELEE_HANDS.cooldown_ticks);
            sim.resolve_attacks(&world);
        }
        assert!(sim.actor_health(dummy).unwrap().is_depleted());
        sim.tick += 300;
        sim.advance_actors(&world, &[]);
        assert_eq!(sim.actor_health(dummy).unwrap(), Health::default());
        assert_eq!(sim.actor_position(dummy).unwrap(), Vec3::new(2.5, 1.0, 0.5));
        put_resources(&mut app, world, sim);
    }

    #[test]
    fn package_weapons_require_ownership_and_apply_their_spec() {
        let mut app = headless_app(1);
        let (_generated, mut sim) = take_resources(&mut app);
        let mut world = empty_world();
        for x in 0..8 {
            for z in 0..8 {
                world.set_block(IVec3::new(x, 0, z), voxel_world::STONE).unwrap();
            }
        }
        let dummy = sim.spawn_actor(Vec3::new(2.5, 1.0, 0.5)).unwrap();
        let mut player = Player::new();
        player.state.position = Vec3::new(2.5, 1.0, 3.5);
        player.input.pitch = 0.0;
        player.input.yaw = 0.0;
        player.input.attack = true;
        // The war hammer is registered by packages/melee but not owned: the
        // swing is refused outright rather than downgraded to hands.
        player.input.selected = 8;
        sim.players.insert(1, player);

        sim.tick = 100;
        sim.resolve_attacks(&world);
        assert_eq!(sim.actor_health(dummy).unwrap().current(), 100);

        sim.players.get_mut(&1).unwrap().inventory.add(8, 1);
        sim.resolve_attacks(&world);
        // Head-zone hit with the authored 30-damage spec.
        assert_eq!(
            sim.actor_health(dummy).unwrap().current(),
            100 - 30 * gameplay::combat::HEADSHOT_MULTIPLIER
        );
        // The authored 60-tick cooldown, not the hands default, gates the next swing.
        sim.tick += u64::from(gameplay::combat::MELEE_HANDS.cooldown_ticks);
        sim.resolve_attacks(&world);
        assert_eq!(
            sim.actor_health(dummy).unwrap().current(),
            100 - 30 * gameplay::combat::HEADSHOT_MULTIPLIER
        );
        sim.tick += 30;
        sim.resolve_attacks(&world);
        assert!(sim.actor_health(dummy).unwrap().is_depleted());
        put_resources(&mut app, world, sim);
    }

    #[test]
    fn controller_actors_use_authoritative_world_and_one_tick_per_update() {
        let mut app = headless_app(1);
        let spawn = app.world().resource::<Simulation>().spawn;
        let position =
            terrain_stream::available_spawn(app.world().resource::<VoxelWorld>(), spawn, &[])
                .unwrap();
        let initial = controller::CharacterState {
            motion: PlayerState {
                position,
                ..Default::default()
            },
            yaw: 0.0,
        };
        let body = controller::CharacterBody::default();
        let profile = controller::MovementProfile::default();
        let mut intent = controller::CharacterIntent {
            movement: glam::Vec2::X,
            ..Default::default()
        };
        let entity = app.world_mut().spawn((initial, body, profile, intent)).id();
        // Host refresh must replace stale observations before the motor runs.
        app.world_mut()
            .resource_mut::<controller::ObservedBodies>()
            .0
            .push(physics::DynamicCollider::cube(99, position, Vec3::ZERO));
        app.update();
        assert_eq!(app.world().resource::<Simulation>().tick, 1);
        assert!(
            app.world()
                .resource::<controller::ObservedBodies>()
                .0
                .is_empty()
        );
        let mut expected = initial;
        controller::step_character(
            app.world().resource::<VoxelWorld>(),
            &mut expected,
            &body,
            &profile,
            &mut intent,
            FIXED_DT,
            &[],
        );
        assert_eq!(
            *app.world()
                .get::<controller::CharacterState>(entity)
                .unwrap(),
            expected
        );
        assert!(expected.motion.position.x > initial.motion.position.x);
    }

    fn headless_app(radius: i32) -> App {
        let mut app = App::new();
        app.add_plugins(
            SimulationPlugin::headless(ServerConfig {
                radius,
                metrics_every: 0,
                ..Default::default()
            })
            .unwrap(),
        );
        app
    }

    fn take_resources(app: &mut App) -> (VoxelWorld, Simulation) {
        let world = app.world_mut().remove_resource::<VoxelWorld>().unwrap();
        let sim = app.world_mut().remove_resource::<Simulation>().unwrap();
        (world, sim)
    }

    fn put_resources(app: &mut App, world: VoxelWorld, sim: Simulation) {
        app.world_mut().insert_resource(world);
        app.world_mut().insert_resource(sim);
    }

    #[test]
    fn headless_startup_pins_package_generator_and_spawn_chunks() {
        let loaded = game_packages::load_terrain(&game_packages::default_directory()).unwrap();
        let app = headless_app(1);
        let spawn_chunks = app.world().resource::<Simulation>().spawn_chunks.clone();
        assert_eq!(spawn_chunks.len(), 27);
        let world = app.world().resource::<VoxelWorld>();
        assert_eq!(world.generator.identity(), loaded.identity());
        assert_eq!(world.generator.version(), loaded.version());
        assert!(
            spawn_chunks
                .iter()
                .all(|coord| world.chunks.contains_key(coord))
        );
    }
    #[test]
    fn startup_spawns_titan_unless_disabled() {
        let app = headless_app(1);
        let names: Vec<&str> = app
            .world()
            .resource::<Simulation>()
            .actors
            .values()
            .map(|actor| actor.kind.name)
            .collect();
        assert_eq!(names, ["dummy", "titan"]);

        let mut app = App::new();
        app.add_plugins(
            SimulationPlugin::headless(ServerConfig {
                radius: 1,
                metrics_every: 0,
                spawn_titan: false,
                ..Default::default()
            })
            .unwrap(),
        );
        let names: Vec<&str> = app
            .world()
            .resource::<Simulation>()
            .actors
            .values()
            .map(|actor| actor.kind.name)
            .collect();
        assert_eq!(names, ["dummy"]);
    }


    #[test]
    fn generated_completion_replays_latest_journal_and_never_overwrites_loaded() {
        let mut app = headless_app(2);
        let (mut world, mut sim) = take_resources(&mut app);
        let seed = world.seed;
        let generator = world.generator.clone();
        let coord = IVec3::new(3, 0, 3);
        let target = coord * CHUNK_SIZE + IVec3::new(4, 4, 4);
        let cell = index(local_coord(target)) as u16;
        sim.needed.insert(coord);
        sim.journal.insert(
            coord,
            Journal {
                revision: 501,
                voxels: BTreeMap::from([(cell, Voxel::placed(5))]),
            },
        );

        // A chunk edited while generation was in flight is left untouched.
        world.insert(coord, Chunk::from_runs(77, &[(32768, 3)]).unwrap());
        sim.install_generated(
            &mut world,
            coord,
            Chunk::generate_with(coord, seed, &generator),
            generator.scatter_chunk(coord, seed),
        );
        assert_eq!(world.chunks[&coord].revision, 77);
        assert_eq!(world.block(target), Some(3));

        // Eviction and regeneration replay the journal edit at its recorded revision.
        world.remove(coord);
        sim.install_generated(
            &mut world,
            coord,
            Chunk::generate_with(coord, seed, &generator),
            generator.scatter_chunk(coord, seed),
        );
        assert_eq!(world.block(target), Some(5));
        assert_eq!(world.chunks[&coord].revision, 501);
        let generated = sim.metrics.generated;

        // A later stale completion for the now-resident chunk is ignored.
        sim.install_generated(
            &mut world,
            coord,
            Chunk::generate_with(coord, seed, &generator),
            generator.scatter_chunk(coord, seed),
        );
        assert_eq!(world.chunks[&coord].revision, 501);
        assert_eq!(sim.metrics.generated, generated);

        put_resources(&mut app, world, sim);
    }

    #[test]
    fn generated_completion_for_unneeded_coord_is_ignored() {
        let mut app = headless_app(1);
        let (mut world, mut sim) = take_resources(&mut app);
        let seed = world.seed;
        let generator = world.generator.clone();
        let coord = IVec3::new(9, 0, 9);
        assert!(!sim.needed.contains(&coord));
        let before = sim.metrics.generated;
        sim.install_generated(
            &mut world,
            coord,
            Chunk::generate_with(coord, seed, &generator),
            generator.scatter_chunk(coord, seed),
        );
        assert!(!world.chunks.contains_key(&coord));
        assert_eq!(sim.metrics.generated, before);
        put_resources(&mut app, world, sim);
    }

    #[test]
    fn dead_player_freezes_movement_input_and_body_push() {
        let mut app = headless_app(1);
        let (world, mut sim) = take_resources(&mut app);
        let mut player = Player::new();
        player.health.damage(u16::MAX);
        player.state.position = Vec3::new(0.5, 40.0, 0.5);
        let position = player.state.position;
        player.body_push_velocity = Vec3::new(3.0, 0.0, 0.0);
        player.input.movement = [1.0, 0.0];
        player.enqueue(PlayerInput {
            sequence: 1,
            movement: [1.0, 0.0],
            jump: true,
            ..Default::default()
        });
        sim.players.insert(1, player);
        put_resources(&mut app, world, sim);
        app.update();
        let sim = app.world().resource::<Simulation>();
        let player = &sim.players[&1];
        assert!(player.health.is_depleted());
        assert_eq!(player.state.position, position);
        assert_eq!(player.state.velocity, Vec3::ZERO);
        assert_eq!(player.body_push_velocity, Vec3::ZERO);
        assert_eq!(player.last_input, 1);
        assert!(player.pending.is_empty());
    }

    #[test]
    fn input_packets_from_a_stale_life_are_ignored() {
        let mut app = headless_app(1);
        let (world, mut sim) = take_resources(&mut app);
        let mut player = Player::new();
        player.life = 2;
        sim.players.insert(1, player);
        sim.accept_inputs(
            1,
            InputPacket {
                session: 0,
                life: 1,
                inputs: vec![PlayerInput {
                    sequence: 1,
                    movement: [1.0, 0.0],
                    ..Default::default()
                }],
            },
        );
        assert!(sim.players[&1].pending.is_empty());
        sim.accept_inputs(
            1,
            InputPacket {
                session: 0,
                life: 2,
                inputs: vec![PlayerInput {
                    sequence: 2,
                    movement: [1.0, 0.0],
                    ..Default::default()
                }],
            },
        );
        assert_eq!(sim.players[&1].pending.len(), 1);
        put_resources(&mut app, world, sim);
    }

    fn drop_arena(world: &mut VoxelWorld, sim: &Simulation) -> (IVec3, IVec3) {
        let coord = *sim
            .spawn_chunks
            .iter()
            .min_by_key(|coord| (coord.x, coord.y, coord.z))
            .unwrap();
        let ground = coord * CHUNK_SIZE + IVec3::new(5, 5, 5);
        let floor = ground - IVec3::Y;
        for dx in -1..=1 {
            for dz in -1..=1 {
                let _ = world.set_block(floor + IVec3::new(dx, 0, dz), voxel_world::STONE);
                for lift in 0..=3 {
                    let _ = world.set_block(ground + IVec3::new(dx, lift, dz), 0);
                }
            }
        }
        (floor, ground)
    }

    #[test]
    fn destroyed_blocks_drop_items_that_fall_settle_and_are_collected() {
        let mut app = headless_app(1);
        let (mut world, mut sim) = take_resources(&mut app);
        let (floor, ground) = drop_arena(&mut world, &sim);

        sim.destroyed(ground, voxel_world::STONE, None);
        assert_eq!(sim.drops.len(), 1);
        assert_eq!(sim.drops[0].snapshot.item, u32::from(voxel_world::STONE));
        assert_eq!(
            sim.drops[0].snapshot.position,
            ground.as_vec3() + Vec3::splat(0.5)
        );

        for _ in 0..300 {
            sim.advance_drops(&world);
        }
        let settled = sim.drops[0].snapshot.position;
        assert!(
            (settled.y - (floor.y as f32 + 1.15)).abs() < 0.05,
            "{settled:?}"
        );
        for _ in 0..60 {
            assert!(!sim.drops[0].step(&world));
            assert_eq!(sim.drops[0].snapshot.position, settled);
        }
        let support = (settled - Vec3::Y * 0.16).floor().as_ivec3();
        world.set_block(support, voxel_world::AIR).unwrap();
        assert!(sim.drops[0].step(&world));
        for _ in 0..12 {
            sim.drops[0].step(&world);
        }
        assert!(sim.drops[0].snapshot.position.y < settled.y);

        let mut player = Player::new();
        player.state.position = ground.as_vec3() + Vec3::new(0.5, 0.0, 0.5);
        sim.players.insert(1, player);
        sim.collect_drops();
        assert!(sim.drops.is_empty());
        assert_eq!(sim.players[&1].inventory.count(u32::from(voxel_world::STONE)), 1);
        assert!(sim.players[&1].inventory_dirty);
        put_resources(&mut app, world, sim);
    }

    #[test]
    fn spawn_drop_rejects_invalid_items_and_bounds_the_set() {
        let mut app = headless_app(1);
        let (_world, mut sim) = take_resources(&mut app);
        for (item, count, position) in [
            (0_u32, 1_u32, Vec3::ZERO),
            (10, 1, Vec3::ZERO),
            (3, 0, Vec3::ZERO),
            (3, 1, Vec3::new(f32::NAN, 0.0, 0.0)),
        ] {
            sim.spawn_drop(position, item, count);
        }
        assert!(sim.drops.is_empty());
        for _ in 0..protocol::MAX_DROPS + 5 {
            sim.spawn_drop(Vec3::ZERO, u32::from(voxel_world::STONE), 1);
        }
        assert_eq!(sim.drops.len(), protocol::MAX_DROPS);
        assert!(
            sim.drops
                .iter()
                .map(|drop| drop.snapshot.id)
                .collect::<Vec<_>>()
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert_eq!(sim.next_drop, protocol::MAX_DROPS as u32 + 6);
    }

    #[test]
    fn collection_merges_into_uncapped_stacks() {
        let mut app = headless_app(1);
        let (mut world, mut sim) = take_resources(&mut app);
        let (_floor, ground) = drop_arena(&mut world, &sim);

        let mut player = Player::new();
        player.state.position = ground.as_vec3() + Vec3::new(0.5, 0.0, 0.5);
        player.inventory.add(u32::from(voxel_world::STONE), 1_000_000);
        sim.players.insert(1, player);
        sim.spawn_drop(ground.as_vec3() + Vec3::splat(0.5), u32::from(voxel_world::STONE), 3);
        for _ in 0..40 {
            sim.advance_drops(&world);
        }
        sim.collect_drops();
        // Uncapped stacks take the whole drop; nothing is left behind.
        assert!(sim.drops.is_empty());
        assert_eq!(
            sim.players[&1].inventory.count(u32::from(voxel_world::STONE)),
            1_000_003
        );
        assert!(sim.players[&1].inventory_dirty);
        put_resources(&mut app, world, sim);
    }

    #[test]
    fn equipment_pickups_take_separate_slots_even_when_owned() {
        let mut app = headless_app(1);
        let (mut world, mut sim) = take_resources(&mut app);
        let (_floor, ground) = drop_arena(&mut world, &sim);

        let mut player = Player::new();
        player.state.position = ground.as_vec3() + Vec3::new(0.5, 0.0, 0.5);
        player.inventory.add_equipment(7);
        sim.players.insert(1, player);
        // A second knife drop is collected, not refused or merged.
        sim.spawn_drop(ground.as_vec3() + Vec3::splat(0.5), 7, 1);
        for _ in 0..40 {
            sim.advance_drops(&world);
        }
        sim.collect_drops();
        assert!(sim.drops.is_empty());
        assert_eq!(sim.players[&1].inventory.count(7), 2);
        assert_eq!(
            sim.players[&1]
                .inventory
                .entries()
                .iter()
                .filter(|&&(item, _)| item == 7)
                .count(),
            2
        );
        put_resources(&mut app, world, sim);
    }

    #[test]
    fn event_backlog_stays_bounded_and_keeps_newest_events() {
        let mut app = headless_app(1);
        let (world, mut sim) = take_resources(&mut app);
        for index in 0..(actors::MAX_QUEUED_EVENTS + 7) {
            sim.push_event(SimEvent::DamageDealt {
                source: SimEntity::Actor(1),
                target: SimEntity::Player(index as u64),
                amount: 1,
                killed: false,
            });
        }
        assert_eq!(sim.events.len(), actors::MAX_QUEUED_EVENTS);
        // Real ticks with nothing to drain must not clear or overflow it.
        put_resources(&mut app, world, sim);
        for _ in 0..5 {
            app.update();
        }
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        assert_eq!(sim.events.len(), actors::MAX_QUEUED_EVENTS);
        let drained = sim.drain_events();
        // Oldest seven events were dropped; the backlog holds 7..=4102.
        assert_eq!(drained.len(), actors::MAX_QUEUED_EVENTS);
        let expected = |index: u64| SimEvent::DamageDealt {
            source: SimEntity::Actor(1),
            target: SimEntity::Player(index),
            amount: 1,
            killed: false,
        };
        assert_eq!(drained[0], expected(7));
        assert_eq!(
            drained[actors::MAX_QUEUED_EVENTS - 1],
            expected(actors::MAX_QUEUED_EVENTS as u64 + 6)
        );
        assert!(sim.events.is_empty());
    }

    #[test]
    fn player_melee_kill_emits_damage_and_player_death_with_player_attribution() {
        let mut app = headless_app(1);
        let (_generated, mut sim) = take_resources(&mut app);
        let mut world = empty_world();
        for x in 0..8 {
            for z in 0..8 {
                world.set_block(IVec3::new(x, 0, z), voxel_world::STONE).unwrap();
            }
        }
        let mut attacker = Player::new();
        attacker.state.position = Vec3::new(2.5, 1.0, 3.5);
        attacker.input.yaw = 0.0; // -Z faces the victim
        attacker.input.attack = true;
        sim.players.insert(1, attacker);
        let mut victim = Player::new();
        victim.state.position = Vec3::new(2.5, 1.0, 0.5);
        sim.players.insert(2, victim);

        sim.tick = 100;
        loop {
            sim.resolve_attacks(&world);
            if sim.player_health(2).unwrap().is_depleted() {
                break;
            }
            sim.tick += u64::from(gameplay::combat::MELEE_HANDS.cooldown_ticks);
        }
        let events = sim.drain_events();
        assert!(
            events.iter().any(|event| matches!(
                event,
                SimEvent::DamageDealt {
                    source: SimEntity::Player(1),
                    target: SimEntity::Player(2),
                    killed: true,
                    ..
                }
            )),
            "no player-attributed kill damage in {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                SimEvent::PlayerDied {
                    id: 2,
                    killer: Some(SimEntity::Player(1)),
                }
            )),
            "no player death event in {events:?}"
        );
        put_resources(&mut app, world, sim);
    }

}
