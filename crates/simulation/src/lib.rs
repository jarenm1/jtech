use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use glam::{IVec3, Vec3};
use networking::ServerTransport;
use physics::{
    EYE_HEIGHT, FIXED_DT, PlayerInput, PlayerState, look_direction, overlaps_block, step_player,
};
use protocol::{ClientMessage, PlayerSnapshot, ServerMessage, Snapshot};
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
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub seed: u64,
    pub radius: i32,
    pub metrics_every: u64,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:4000".parse().unwrap(),
            seed: 7,
            radius: 3,
            metrics_every: 600,
        }
    }
}
pub struct SimulationPlugin {
    config: ServerConfig,
    transport: parking_lot::Mutex<Option<ServerTransport>>,
}
impl SimulationPlugin {
    pub fn bind(mut config: ServerConfig) -> io::Result<Self> {
        config.radius = config.radius.clamp(1, 6);
        let transport = ServerTransport::bind(config.bind)?;
        config.bind = transport.local_addr()?;
        Ok(Self {
            config,
            transport: parking_lot::Mutex::new(Some(transport)),
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
        app.insert_resource(world)
            .insert_resource(Simulation {
                config: self.config.clone(),
                transport: self.transport.lock().take().expect("plugin added twice"),
                players: HashMap::new(),
                journal: HashMap::new(),
                journal_blocks: 0,
                last_needed: HashMap::new(),
                tick: 0,
                metrics: SimulationMetrics::default(),
                tick_times: VecDeque::new(),
                needed: HashSet::new(),
            })
            .add_systems(Update, advance);
    }
}
struct Player {
    state: PlayerState,
    last_input: u64,
    input: PlayerInput,
    pending: BTreeMap<u64, PlayerInput>,
    known: HashMap<IVec3, u64>,
    interest: HashSet<IVec3>,
    safety: HashSet<IVec3>,
    interest_center: Option<IVec3>,
    last_edit: u64,
    results: VecDeque<(u64, bool)>,
    highest_request: u64,
}
impl Player {
    fn new() -> Self {
        Self {
            state: PlayerState::default(),
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
        }
    }
    fn snapshot(&self, id: u64) -> PlayerSnapshot {
        PlayerSnapshot {
            id,
            last_input: self.last_input,
            state: self.state,
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
}
#[derive(Resource)]
pub struct Simulation {
    config: ServerConfig,
    transport: ServerTransport,
    players: HashMap<u64, Player>,
    journal: HashMap<IVec3, Journal>,
    journal_blocks: usize,
    last_needed: HashMap<IVec3, u64>,
    pub tick: u64,
    pub metrics: SimulationMetrics,
    tick_times: VecDeque<u64>,
    needed: HashSet<IVec3>,
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
            self.transport.stats.received_bytes,
            self.transport.stats.sent_bytes,
            self.transport.stats.rejected_datagrams,
            percentile(50),
            percentile(95),
            percentile(99),
            percentile(100)
        );
    }
    fn drop_player(&mut self, id: u64) {
        self.transport.disconnect(id);
        if self.players.remove(&id).is_some() {
            self.metrics.disconnected += 1;
            eprintln!("disconnect id={id}");
        }
    }
    fn send(&mut self, id: u64, message: &ServerMessage) -> bool {
        if self.transport.send(id, message).is_err() {
            self.drop_player(id);
            false
        } else {
            true
        }
    }
    fn edit(
        &mut self,
        world: &mut VoxelWorld,
        id: u64,
        request: u64,
        target: IVec3,
        block: u8,
        expected_revision: u64,
    ) {
        let Some(player) = self.players.get(&id) else {
            return;
        };
        if let Some((_, accepted)) = player.results.iter().find(|(old, _)| *old == request) {
            let result = ServerMessage::EditResult {
                request,
                accepted: *accepted,
            };
            self.send(id, &result);
            return;
        }
        let coord = chunk_coord(target);
        let valid = request > player.highest_request
            && self.tick >= player.last_edit.saturating_add(6)
            && block <= 5
            && target.y > MIN_CHUNK_Y * CHUNK_SIZE
            && valid_coord(coord)
            && player.interest.contains(&coord)
            && player.known.get(&coord) == Some(&expected_revision)
            && world
                .chunks
                .get(&coord)
                .is_some_and(|chunk| chunk.revision == expected_revision)
            && world.block(target).is_some_and(|existing| {
                if block == 0 {
                    existing != 0
                } else {
                    existing == 0
                }
            })
            && (block == 0
                || !self
                    .players
                    .values()
                    .any(|other| overlaps_block(&other.state, target)))
            && world
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
            && (self.journal_blocks < MAX_JOURNAL_BLOCKS
                || self
                    .journal
                    .get(&coord)
                    .is_some_and(|j| j.blocks.contains_key(&(index(local_coord(target)) as u16))));
        let changed = if valid {
            world.set_block(target, block)
        } else {
            None
        };
        let accepted = changed.is_some();
        if let Some(player) = self.players.get_mut(&id) {
            player.highest_request = player.highest_request.max(request);
            player.last_edit = self.tick;
            player.results.push_back((request, accepted));
            if player.results.len() > 64 {
                player.results.pop_front();
            }
        }
        if let Some((from, to)) = changed {
            self.metrics.accepted_edits += 1;
            let local_index = index(local_coord(target)) as u16;
            let journal = self.journal.entry(coord).or_default();
            journal.revision = to;
            if journal.blocks.insert(local_index, block).is_none() {
                self.journal_blocks += 1;
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
        } else {
            self.metrics.rejected_edits += 1;
        }
        eprintln!("edit id={id} request={request} accepted={accepted} target={target}");
        self.send(id, &ServerMessage::EditResult { request, accepted });
    }
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
    let incoming = match sim.transport.poll() {
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
        sim.players.insert(id, player);
        sim.send(
            id,
            &ServerMessage::Welcome {
                id,
                session,
                seed: sim.config.seed,
                spawn,
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
        step_player(&world, &mut player.state, &input, FIXED_DT);
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
            } => sim.edit(&mut world, id, request, target, block, expected_revision),
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
    let mut needed = std::mem::take(&mut sim.needed);
    needed.clear();
    // One chunk of safety beyond visible interest ensures swept bodies never reach unloaded edges.
    for player in sim.players.values() {
        needed.extend(player.safety.iter().copied());
    }
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
        sim.players
            .values()
            .map(|p| {
                let delta = *coord - chunk_coord(p.state.position.floor().as_ivec3());
                delta.as_vec3().length_squared() as u64
            })
            .min()
            .unwrap_or(0)
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
            if sim.transport.snapshot(id, &snapshot).is_err() {
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
}
