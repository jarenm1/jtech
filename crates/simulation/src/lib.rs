mod active_terrain;
#[cfg(test)]
mod bombardment_tests;
mod bow;
mod bow_server;
mod explosion;
mod health;
mod material_damage;
mod physics_slice;
use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use gameplay::Health;
use glam::{IVec3, Vec3};
use material_damage::{MAX_DAMAGED_BLOCKS, tool_contact};
use networking::ServerTransport;
use physics::{
    EYE_HEIGHT, FIXED_DT, PlayerInput, PlayerState, look_direction, overlaps_block, step_player,
};
use physics_slice::PhysicsSlice;
use protocol::{ClientMessage, EditRejection, PlayerSnapshot, ServerMessage, Snapshot};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    io,
    net::SocketAddr,
    time::Instant,
};
use voxel_world::{
    CHUNK_SIZE, Chunk, MAX_CHUNK_Y, MIN_CHUNK_Y, VoxelWorld, chunk_coord, index, local_coord,
};

const MAX_INPUT_QUEUE: usize = 32;
const MAX_JOURNAL_BLOCKS: usize = 1_000_000;
const MAX_QUEUED_STRIKES: usize = 4;
const STRIKE_QUEUE_TICKS: u64 = 60;

type EditOutcome = Result<Option<f32>, EditRejection>;

#[derive(Clone, Copy, Debug)]
struct DamageState {
    material: u8,
    joules: f32,
    release: bool,
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
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:4000".parse().unwrap(),
            seed: 7,
            radius: 3,
            metrics_every: 600,
            gpu_physics: false,
        }
    }
}
pub struct SimulationPlugin {
    config: ServerConfig,
    transport: parking_lot::Mutex<Option<ServerTransport>>,
    physics: parking_lot::Mutex<Option<PhysicsSlice>>,
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
        config.radius = config.radius.clamp(1, 6);
        let physics = if config.gpu_physics {
            Some(PhysicsSlice::new().map_err(io::Error::other)?)
        } else {
            None
        };
        Ok(Self {
            config,
            transport: parking_lot::Mutex::new(None),
            physics: parking_lot::Mutex::new(physics),
        })
    }
    pub fn local_addr(&self) -> SocketAddr {
        self.config.bind
    }
}
impl Plugin for SimulationPlugin {
    fn build(&self, app: &mut App) {
        let mut world = VoxelWorld {
            seed: self.config.seed,
            ..Default::default()
        };
        for y in MIN_CHUNK_Y..=MAX_CHUNK_Y {
            for z in -1..=1 {
                for x in -1..=1 {
                    world.ensure_chunk(IVec3::new(x, y, z));
                }
            }
        }
        let physics = self.physics.lock().take();
        app.insert_resource(world)
            .insert_resource(Simulation {
                config: self.config.clone(),
                transport: self.transport.lock().take(),
                players: HashMap::new(),
                journal: HashMap::new(),
                journal_blocks: 0,
                damage: HashMap::new(),
                strikes: VecDeque::new(),
                arrows: Vec::new(),
                detonations: VecDeque::new(),
                next_arrow: 1,
                arrow_revision: 0,
                last_needed: HashMap::new(),
                tick: 0,
                metrics: SimulationMetrics::default(),
                tick_times: VecDeque::new(),
                needed: HashSet::new(),
                physics,
            })
            .add_systems(Update, advance);
    }
}
struct Player {
    state: PlayerState,
    health: Health,
    last_input: u64,
    input: PlayerInput,
    pending: BTreeMap<u64, PlayerInput>,
    known: HashMap<IVec3, u64>,
    interest: HashSet<IVec3>,
    safety: HashSet<IVec3>,
    interest_center: Option<IVec3>,
    last_edit: u64,
    results: VecDeque<(u64, EditOutcome)>,
    highest_request: u64,
    physics_revision: Option<u64>,
    body_push_velocity: Vec3,
    next_bow_tick: u64,
    arrow_revision: Option<u64>,
}
impl Player {
    fn new() -> Self {
        Self {
            state: PlayerState::default(),
            health: Health::default(),
            last_input: 0,
            input: PlayerInput::default(),
            pending: BTreeMap::new(),
            known: HashMap::new(),
            interest: HashSet::new(),
            safety: HashSet::new(),
            interest_center: None,
            last_edit: 0,
            results: VecDeque::new(),
            highest_request: 0,
            physics_revision: None,
            body_push_velocity: Vec3::ZERO,
            next_bow_tick: 0,
            arrow_revision: None,
        }
    }
    fn snapshot(&self, id: u64) -> PlayerSnapshot {
        PlayerSnapshot {
            id,
            last_input: self.last_input,
            state: self.state,
            health: self.health,
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
        if self.pending.len() == MAX_INPUT_QUEUE {
            return;
        }
        self.pending.insert(input.sequence, input);
    }
}
#[derive(Default)]
struct Journal {
    revision: u64,
    blocks: BTreeMap<u16, u8>,
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
    physics: Option<PhysicsSlice>,
    /// Sparse exceptions survive chunk eviction and transfer across loose/grid ownership.
    damage: HashMap<IVec3, DamageState>,
    strikes: VecDeque<QueuedStrike>,
    arrows: Vec<bow::Arrow>,
    detonations: VecDeque<(u32, Vec3)>,
    next_arrow: u32,
    arrow_revision: u64,
}
impl Simulation {
    pub fn player_count(&self) -> usize {
        self.players.len()
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
                .is_some_and(|j| j.blocks.contains_key(&(index(local_coord(target)) as u16)))
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
        if strike {
            let rejection = self
                .physics
                .as_ref()
                .map_or(Some(EditRejection::PhysicsUnavailable), |p| {
                    p.detach_rejection(target)
                });
            if let Some(reason) = rejection {
                self.finish_edit(id, request, Err(reason));
                return;
            }
            if self.physics.as_ref().is_some_and(|p| p.is_busy()) {
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
        if !self.has_journal_space(target) {
            return Err(EditRejection::StorageFull);
        }
        if block != 0
            && (self
                .players
                .values()
                .any(|p| overlaps_block(&p.state, target))
                || self.physics.as_ref().is_some_and(|p| !p.can_place(target)))
        {
            return Err(EditRejection::Occupied);
        }
        let detached = if strike {
            let physics = self
                .physics
                .as_ref()
                .ok_or(EditRejection::PhysicsUnavailable)?;
            if let Some(reason) = physics.detach_rejection(target) {
                return Err(reason);
            }
            if physics.is_busy() {
                return Err(EditRejection::PhysicsUnavailable);
            }
            let player = &self.players[&id];
            Some((
                world.block(target).ok_or(EditRejection::InvalidTarget)?,
                look_direction(player.input.yaw, player.input.pitch),
                self.damage.get(&target).map_or(0.0, |state| state.joules),
            ))
        } else {
            None
        };
        let (from, to) = world
            .set_block(target, block)
            .ok_or(EditRejection::RevisionExhausted)?;
        if let Some((material, direction, damage)) = detached {
            // F is a debug energy source; material damage is carried into the body.
            self.physics.as_mut().unwrap().release(
                target,
                material,
                damage,
                direction * 5.0 + Vec3::Y * 20.0,
            );
        }
        self.record_change(world, target, block, from, to);
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
        if let Some((from, to)) = result.destroyed_revision {
            self.record_change(world, target, 0, from, to);
            self.destroyed(target, contact.material as u8, None);
        }
        Ok(result.fraction)
    }

    /// Common destruction event for grid and loose ownership. Item spawning belongs here.
    fn destroyed(&mut self, target: IVec3, material: u8, body: Option<u32>) {
        self.metrics.destroyed_blocks += 1;
        eprintln!("destroyed target={target} material={material} body={body:?}");
    }
    /// One authoritative voxel transaction path for edits, detachment, and settlement.
    fn record_change(&mut self, _world: &VoxelWorld, target: IVec3, block: u8, from: u64, to: u64) {
        self.damage.remove(&target);
        let coord = chunk_coord(target);
        let local_index = index(local_coord(target)) as u16;
        let journal = self.journal.entry(coord).or_default();
        journal.revision = to;
        if journal.blocks.insert(local_index, block).is_none() {
            self.journal_blocks += 1;
        }
        if let Some(physics) = &mut self.physics {
            physics.set_voxel(target, block);
        }
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
                    local_index,
                    block,
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
        // ownership transaction when body capacity/readback permits it.
        let mut releases: Vec<_> = self
            .damage
            .iter()
            .filter(|(_, state)| state.release)
            .map(|(&target, &state)| (target, state))
            .collect();
        releases.sort_unstable_by_key(|(p, _)| (p.y, p.z, p.x));
        for (target, state) in releases {
            if world.block(target) == Some(state.material)
                && physics.can_detach(target)
                && self.has_journal_space(target)
                && let Some((from, to)) = world.set_block(target, 0)
            {
                // The solver already reacted the collision impulse into the grid.
                physics.release(target, state.material, state.joules, Vec3::ZERO);
                physics.set_voxel(target, 0);
                self.record_change(world, target, 0, from, to);
            }
        }
        // Stable body IDs establish deterministic order for competing cell claims.
        for (id, target, material) in physics.settling_candidates() {
            for target in physics.placement_cells(id, target) {
                let coord = chunk_coord(target);
                let has_journal_space = self.journal_blocks < MAX_JOURNAL_BLOCKS
                    || self.journal.get(&coord).is_some_and(|j| {
                        j.blocks.contains_key(&(index(local_coord(target)) as u16))
                    });
                if has_journal_space
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
                        || physics.body_damage(id) == 0.0)
                    && let Some((from, to)) = world.set_block(target, material)
                {
                    let damage = physics.body_damage(id);
                    physics.remove(id);
                    physics.set_voxel(target, material);
                    self.record_change(world, target, material, from, to);
                    if damage > 0.0 {
                        self.damage.insert(
                            target,
                            DamageState {
                                material,
                                joules: damage,
                                release: false,
                            },
                        );
                    }
                    break;
                }
            }
        }
        let mut players: Vec<_> = self.players.iter().collect();
        players.sort_unstable_by_key(|(id, _)| **id);
        physics.player_colliders(
            players
                .into_iter()
                .enumerate()
                .map(|(slot, (_, player))| gpu_physics::PlayerCollider {
                    position: player.state.position.to_array(),
                    id: slot as u32,
                    velocity: player.body_push_velocity.to_array(),
                    padding: 0,
                })
                .collect(),
        );
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
    if block > 5
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
fn interests(position: Vec3, radius: i32) -> HashSet<IVec3> {
    let center = chunk_coord(position.floor().as_ivec3());
    let mut result = HashSet::new();
    for y in MIN_CHUNK_Y..=MAX_CHUNK_Y {
        for z in -radius..=radius {
            for x in -radius..=radius {
                let coord = IVec3::new(center.x + x, y, center.z + z);
                if valid_coord(coord) {
                    result.insert(coord);
                }
            }
        }
    }
    result
}
fn restore(world: &mut VoxelWorld, coord: IVec3, journal: Option<&Journal>) {
    world.ensure_chunk(coord);
    if let Some(journal) = journal {
        for (&cell, &block) in &journal.blocks {
            let cell = cell as i32;
            let local = IVec3::new(cell % 32, cell / 1024, (cell / 32) % 32);
            world.set_block(coord * CHUNK_SIZE + local, block);
        }
        let runs = world.chunks[&coord].runs();
        world.insert(
            coord,
            Chunk::from_runs(journal.revision, &runs).expect("journal contains valid blocks"),
        );
    }
}
fn advance(mut simulation: ResMut<Simulation>, mut world: ResMut<VoxelWorld>) {
    let started = Instant::now();
    let sim = &mut *simulation;
    sim.tick += 1;
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
        let player = Player::new();
        let spawn = player.state;
        let health = player.health;
        sim.players.insert(id, player);
        sim.send(
            id,
            &ServerMessage::Welcome {
                id,
                session,
                seed: sim.config.seed,
                spawn,
                health,
            },
        );
        eprintln!("connect id={id}");
    }
    for (id, packet) in incoming.inputs {
        if let Some(player) = sim.players.get_mut(&id) {
            for input in packet.inputs {
                player.enqueue(input);
            }
        }
    }
    // Only one command advances each actor per server tick, regardless of packet rate.
    let bodies = sim
        .physics
        .as_ref()
        .map(|p| p.dynamic_colliders())
        .unwrap_or_default();
    for player in sim.players.values_mut() {
        let input = if let Some((sequence, input)) = player.pending.pop_first() {
            player.last_input = sequence;
            player.input = input;
            input
        } else {
            PlayerInput {
                movement: [0.0; 2],
                jump: false,
                ..player.input
            }
        };
        let previous = player.state;
        let mut intended = previous;
        step_player(&world, &mut intended, &input, FIXED_DT);
        physics::step_player_with_bodies(&world, &mut player.state, &input, FIXED_DT, &bodies);
        // Preserve attempted horizontal motion when a loose body blocks the character.
        // The GPU resolves that kinematic push against material mass and terrain.
        player.body_push_velocity = Vec3::new(
            intended.velocity.x,
            player.state.velocity.y,
            intended.velocity.z,
        );
        if !player.state.position.is_finite()
            || player.state.position.x.abs() >= 31_999_900.0
            || player.state.position.z.abs() >= 31_999_900.0
        {
            player.state = previous;
            player.state.velocity = Vec3::ZERO;
        }
        let center = chunk_coord(player.state.position.floor().as_ivec3());
        if player.interest_center != Some(center) {
            player.interest = interests(player.state.position, sim.config.radius);
            player.safety = interests(player.state.position, sim.config.radius + 1);
            player.interest_center = Some(center);
        }
    }
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
            } => sim.fire_bow(&world, id, request, yaw, pitch),
            ClientMessage::Resync { coord } => {
                if let Some(player) = sim.players.get_mut(&id) {
                    // Baseline transmission is already capped at two chunks/tick. Retain
                    // every requested repair: dropping a burst here can strand a client
                    // on a stale revision forever.
                    if player.interest.contains(&coord) {
                        player.known.remove(&coord);
                    }
                }
            }
            ClientMessage::Hello { .. } => {}
        }
    }
    sim.drain_strikes(&mut world);
    sim.advance_bow(&mut world);
    sim.advance_physics(&mut world);
    let mut needed = std::mem::take(&mut sim.needed);
    needed.clear();
    // One chunk of safety beyond visible interest ensures swept bodies never reach unloaded edges.
    for player in sim.players.values() {
        needed.extend(player.safety.iter().copied());
    }
    // Body collision residency is independent of visible/player interest.
    let physics_needed = sim
        .physics
        .as_ref()
        .map(PhysicsSlice::needed_chunks)
        .unwrap_or_default();
    needed.extend(physics_needed.iter().copied());
    // Keep spawn warm for reconnects without admitting actors over unloaded terrain.
    for y in MIN_CHUNK_Y..=MAX_CHUNK_Y {
        for z in -1..=1 {
            for x in -1..=1 {
                needed.insert(IVec3::new(x, y, z));
            }
        }
    }
    for &coord in &needed {
        sim.last_needed.insert(coord, sim.tick);
    }
    let mut missing: Vec<_> = needed
        .iter()
        .copied()
        .filter(|coord| !world.chunks.contains_key(coord))
        .collect();
    missing.sort_unstable_by_key(|coord| {
        (
            !physics_needed.contains(coord),
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
    for coord in missing.into_iter().take(2) {
        restore(&mut world, coord, sim.journal.get(&coord));
        sim.metrics.generated += 1;
    }
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
        let mut available: Vec<_> = player
            .interest
            .iter()
            .copied()
            .filter(|coord| !player.known.contains_key(coord) && world.chunks.contains_key(coord))
            .collect();
        available.sort_unstable_by_key(|coord| (*coord - center).length_squared());
        for coord in available.into_iter().take(2) {
            let chunk = &world.chunks[&coord];
            if !sim.send(
                *id,
                &ServerMessage::Chunk {
                    coord,
                    revision: chunk.revision,
                    runs: chunk.runs(),
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
        for id in ids {
            let Some(player) = sim.players.get(&id) else {
                continue;
            };
            let snapshot = Snapshot {
                tick: sim.tick,
                you: player.snapshot(id),
                players: sim
                    .players
                    .iter()
                    .filter(|(other_id, other)| {
                        **other_id != id
                            && player
                                .interest
                                .contains(&chunk_coord(other.state.position.floor().as_ivec3()))
                    })
                    .map(|(&id, p)| p.snapshot(id))
                    .collect(),
            };
            if sim
                .transport
                .as_mut()
                .is_some_and(|net| net.snapshot(id, &snapshot).is_err())
            {
                sim.metrics.snapshot_errors += 1;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn eviction_restores_edit_and_revision_even_after_reverting_cell() {
        let coord = IVec3::ZERO;
        let target = IVec3::new(1, 20, 1);
        let mut world = VoxelWorld::default();
        world.ensure_chunk(coord);
        let original = world.block(target).unwrap();
        let changed = if original == 0 { 3 } else { 0 };
        world.set_block(target, changed).unwrap();
        let (_, revision) = world.set_block(target, original).unwrap();
        let journal = Journal {
            revision,
            blocks: BTreeMap::from([(index(target) as u16, original)]),
        };
        world.remove(coord);
        restore(&mut world, coord, Some(&journal));
        assert_eq!(world.block(target), Some(original));
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
            blocks: BTreeMap::from([(index(source) as u16, 0), (index(destination) as u16, 5)]),
        };
        world.remove(IVec3::ZERO);
        restore(&mut world, IVec3::ZERO, Some(&journal));
        assert_eq!(world.block(source), Some(0));
        assert_eq!(world.block(destination), Some(5));
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
}
