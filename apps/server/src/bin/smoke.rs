use bevy_app::App;
use glam::{IVec3, Vec3};
use networking::ClientTransport;
use physics::{EYE_HEIGHT, FIXED_DT, PlayerInput, PlayerState, step_player};
use protocol::{ClientMessage, InputPacket, ServerMessage};
use simulation::{ServerConfig, Simulation, SimulationPlugin};
use std::{
    collections::{HashMap, VecDeque},
    error::Error,
    net::SocketAddr,
    time::Duration,
};
use voxel_world::{CHUNK_SIZE, Chunk, VoxelWorld, chunk_coord};

type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;
struct Bot {
    net: ClientTransport,
    id: u64,
    session: u64,
    world: VoxelWorld,
    state: PlayerState,
    authority: PlayerState,
    sequence: u64,
    acknowledged: u64,
    history: VecDeque<PlayerInput>,
    delayed: VecDeque<(u64, InputPacket)>,
    snapshot_tick: u64,
    remotes: usize,
    edits: HashMap<u64, bool>,
    chunks: usize,
    deltas: usize,
    forgotten: usize,
    corrections: usize,
    max_error: f32,
    jitter: bool,
}
impl Bot {
    fn connect(address: SocketAddr, jitter: bool) -> Result<Self> {
        Ok(Self {
            net: ClientTransport::connect(address)?,
            id: 0,
            session: 0,
            world: VoxelWorld::default(),
            state: PlayerState::default(),
            authority: PlayerState::default(),
            sequence: 0,
            acknowledged: 0,
            history: VecDeque::new(),
            delayed: VecDeque::new(),
            snapshot_tick: 0,
            remotes: 0,
            edits: HashMap::new(),
            chunks: 0,
            deltas: 0,
            forgotten: 0,
            corrections: 0,
            max_error: 0.0,
            jitter,
        })
    }
    fn poll(&mut self) -> Result<()> {
        let incoming = self.net.poll()?;
        for message in incoming.reliable {
            match message {
                ServerMessage::Welcome {
                    id,
                    session,
                    seed,
                    spawn,
                } => {
                    self.id = id;
                    self.session = session;
                    self.world.seed = seed;
                    self.state = spawn;
                    self.authority = spawn;
                }
                ServerMessage::Chunk {
                    coord,
                    revision,
                    runs,
                } => {
                    self.world.insert(coord, Chunk::from_runs(revision, &runs)?);
                    self.chunks += 1;
                }
                ServerMessage::Delta {
                    coord,
                    from,
                    to,
                    local_index,
                    block,
                } => {
                    if self
                        .world
                        .chunks
                        .get(&coord)
                        .is_some_and(|chunk| chunk.revision == from)
                    {
                        let cell = local_index as i32;
                        let target = coord * CHUNK_SIZE
                            + IVec3::new(cell % 32, cell / 1024, (cell / 32) % 32);
                        let revision = self
                            .world
                            .set_block(target, block)
                            .ok_or("delta did not change terrain")?;
                        if revision != (from, to) {
                            return Err("delta revision mismatch".into());
                        }
                        self.deltas += 1;
                    } else {
                        self.net.send(ClientMessage::Resync { coord })?;
                    }
                }
                ServerMessage::Forget { coord } => {
                    self.world.remove(coord);
                    self.forgotten += 1;
                }
                ServerMessage::EditResult { request, accepted } => {
                    self.edits.insert(request, accepted);
                }
                ServerMessage::Disconnect { reason } => return Err(reason.into()),
            }
        }
        for snapshot in incoming.snapshots {
            if snapshot.tick <= self.snapshot_tick {
                continue;
            }
            if snapshot.you.last_input < self.acknowledged {
                return Err("input acknowledgement regressed".into());
            }
            self.snapshot_tick = snapshot.tick;
            self.acknowledged = snapshot.you.last_input;
            self.remotes = snapshot.players.len();
            self.authority = snapshot.you.state;
            while self
                .history
                .front()
                .is_some_and(|input| input.sequence <= self.acknowledged)
            {
                self.history.pop_front();
            }
            let previous = self.state.position;
            self.state = self.authority;
            for input in &self.history {
                step_player(&self.world, &mut self.state, input, FIXED_DT);
            }
            let error = previous.distance(self.state.position);
            self.max_error = self.max_error.max(error);
            if error > 0.001 {
                self.corrections += 1;
            }
        }
        Ok(())
    }
    fn input(&mut self, tick: u64, movement: [f32; 2], pitch: f32) -> Result<()> {
        if self.session == 0 {
            return Ok(());
        }
        self.sequence += 1;
        let input = PlayerInput {
            sequence: self.sequence,
            movement,
            yaw: 0.0,
            pitch,
            jump: movement != [0.0; 2],
        };
        step_player(&self.world, &mut self.state, &input, FIXED_DT);
        self.history.push_back(input);
        if self.history.len() > 256 {
            return Err("prediction history exceeded bound".into());
        }
        // Three consecutive packet losses, duplicates, and out-of-order arrivals;
        // the last eight commands recover loss without accelerating server time.
        if !self.jitter || tick % 29 >= 3 {
            let packet = InputPacket {
                session: self.session,
                inputs: self
                    .history
                    .iter()
                    .skip(self.history.len().saturating_sub(8))
                    .copied()
                    .collect(),
            };
            let delay = if self.jitter {
                [3, 0, 2, 1][tick as usize % 4]
            } else {
                0
            };
            self.delayed.push_back((tick + delay, packet));
        }
        let mut keep = VecDeque::new();
        while let Some((when, packet)) = self.delayed.pop_front() {
            if when <= tick {
                self.net.send_inputs(packet.clone())?;
                if self.jitter && tick.is_multiple_of(17) {
                    self.net.send_inputs(packet)?;
                }
            } else {
                keep.push_back((when, packet));
            }
        }
        self.delayed = keep;
        Ok(())
    }
}
fn drive(
    app: &mut App,
    bots: &mut [&mut Bot],
    steps: usize,
    movement: [f32; 2],
    pitch: f32,
) -> Result<()> {
    for _ in 0..steps {
        let tick = app.world().resource::<Simulation>().tick;
        // Move peers in opposite directions to exercise disjoint interest sets.
        for (index, bot) in bots.iter_mut().enumerate() {
            let movement = if index % 2 == 0 {
                movement
            } else {
                [-movement[0], -movement[1]]
            };
            bot.poll()?;
            bot.input(tick, movement, pitch)?;
            bot.poll()?;
        }
        app.update();
        // Yield to the real TCP/IP stack; simulation time remains exact fixed60.
        std::thread::sleep(Duration::from_millis(1));
        for bot in bots.iter_mut() {
            bot.poll()?;
        }
    }
    Ok(())
}
fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn main() -> Result<()> {
    let plugin = SimulationPlugin::bind(ServerConfig {
        bind: "127.0.0.1:0".parse()?,
        radius: 1,
        metrics_every: 0,
        ..Default::default()
    })?;
    let address = plugin.local_addr();
    let mut app = App::new();
    app.add_plugins(plugin);
    let mut first = Bot::connect(address, true)?;
    let mut second = Bot::connect(address, false)?;
    drive(
        &mut app,
        &mut [&mut first, &mut second],
        240,
        [0.0; 2],
        -1.5,
    )?;
    require(
        first.id != 0 && second.id != 0 && first.id != second.id,
        "two distinct players were not admitted",
    )?;
    require(
        first.remotes == 1 && second.remotes == 1,
        "remote actor replication missing",
    )?;
    require(
        first.state.grounded && second.state.grounded,
        "actors did not settle on terrain",
    )?;
    require(
        first.world.chunks.len() == 27 && second.world.chunks.len() == 27,
        "bounded initial interest did not finish streaming",
    )?;
    let hit = first
        .world
        .raycast(
            first.authority.position + Vec3::Y * EYE_HEIGHT,
            physics::look_direction(0.0, -1.5),
            6.0,
        )
        .ok_or("no editable ground in reach")?;
    let coord = chunk_coord(hit.block);
    let before = first.world.chunks[&coord].revision;
    let edit = ClientMessage::Edit {
        request: 1,
        target: hit.block,
        block: 0,
        expected_revision: before,
    };
    first.net.send(edit.clone())?;
    drive(&mut app, &mut [&mut first, &mut second], 30, [0.0; 2], -1.5)?;
    require(
        first.edits.get(&1) == Some(&true),
        "valid edit was rejected",
    )?;
    require(
        first.deltas == 1 && second.deltas == 1,
        "edit was not replicated once to both baselines",
    )?;
    require(
        first.world.chunks[&coord].revision == before + 1
            && second.world.chunks[&coord].revision == before + 1,
        "clients disagree on terrain revision",
    )?;
    first.net.send(edit)?;
    drive(&mut app, &mut [&mut first, &mut second], 12, [0.0; 2], -1.5)?;
    require(first.deltas == 1, "duplicate request applied a second edit")?;
    first.net.send(ClientMessage::Edit {
        request: 2,
        target: hit.block,
        block: 3,
        expected_revision: before,
    })?;
    drive(&mut app, &mut [&mut first, &mut second], 12, [0.0; 2], -1.5)?;
    require(
        first.edits.get(&2) == Some(&false),
        "stale edit revision was accepted",
    )?;
    first.net.send(ClientMessage::Edit {
        request: 3,
        target: hit.block + IVec3::X * 1000,
        block: 255,
        expected_revision: before + 1,
    })?;
    drive(&mut app, &mut [&mut first, &mut second], 12, [0.0; 2], -1.5)?;
    require(
        first.edits.get(&3) == Some(&false),
        "invalid out-of-reach edit was accepted",
    )?;
    // Deliberately discard a baseline then request authoritative replacement.
    first.world.remove(coord);
    first.net.send(ClientMessage::Resync { coord })?;
    drive(&mut app, &mut [&mut first, &mut second], 24, [0.0; 2], -1.5)?;
    require(
        first
            .world
            .chunks
            .get(&coord)
            .is_some_and(|chunk| chunk.revision == before + 1),
        "resync did not restore current revision",
    )?;
    // A burst of repair requests must restore every baseline, not silently drop
    // all but one because of a per-player resync cooldown.
    let repairs = [coord, coord + IVec3::X, coord + IVec3::Z];
    for repair in repairs {
        first.world.remove(repair);
        first.net.send(ClientMessage::Resync { coord: repair })?;
    }
    drive(&mut app, &mut [&mut first, &mut second], 30, [0.0; 2], -1.5)?;
    require(
        repairs.iter().all(|c| first.world.chunks.contains_key(c)),
        "burst resync stranded a missing baseline",
    )?;
    // Place a block ahead of the players, not inside their collision bodies.
    let placement_pitch = -0.65;
    drive(
        &mut app,
        &mut [&mut first, &mut second],
        30,
        [0.0; 2],
        placement_pitch,
    )?;
    let placement = first
        .world
        .raycast(
            first.authority.position + Vec3::Y * EYE_HEIGHT,
            physics::look_direction(0.0, placement_pitch),
            6.0,
        )
        .ok_or("no placement surface in reach")?
        .adjacent;
    require(
        !physics::overlaps_block(&first.authority, placement)
            && !physics::overlaps_block(&second.authority, placement),
        "placement scenario intersects a player",
    )?;
    let placement_coord = chunk_coord(placement);
    let placement_revision = first.world.chunks[&placement_coord].revision;
    first.net.send(ClientMessage::Edit {
        request: 4,
        target: placement,
        block: 5,
        expected_revision: placement_revision,
    })?;
    drive(
        &mut app,
        &mut [&mut first, &mut second],
        30,
        [0.0; 2],
        placement_pitch,
    )?;
    require(
        first.edits.get(&4) == Some(&true),
        "valid placement was rejected",
    )?;
    require(
        first.world.block(placement) == Some(5) && second.world.block(placement) == Some(5),
        "placement did not replicate to both clients",
    )?;
    // Remove the block we just placed through the same authoritative path:
    // it is directly ahead and forms a two-block wall above the excavated floor.
    first.net.send(ClientMessage::Edit {
        request: 5,
        target: placement,
        block: 0,
        expected_revision: first.world.chunks[&placement_coord].revision,
    })?;
    drive(
        &mut app,
        &mut [&mut first, &mut second],
        30,
        [0.0; 2],
        placement_pitch,
    )?;
    require(
        first.edits.get(&5) == Some(&true)
            && first.world.block(placement) == Some(0)
            && second.world.block(placement) == Some(0),
        "placed-block removal did not replicate",
    )?;
    let final_revision = first.world.chunks[&coord].revision;
    let started = first.authority.position;
    drive(
        &mut app,
        &mut [&mut first, &mut second],
        960,
        [0.0, 1.0],
        0.0,
    )?;
    drive(&mut app, &mut [&mut first, &mut second], 90, [0.0; 2], 0.0)?;
    require(
        first.authority.position.distance(started) > 32.0,
        &format!(
            "movement did not cross a chunk boundary: start={started} end={} ack={}",
            first.authority.position, first.acknowledged
        ),
    )?;
    require(
        first.forgotten > 0 && first.chunks > 27,
        "interest did not forget and stream chunks",
    )?;
    require(
        first.world.chunks.len() <= 27 && second.world.chunks.len() <= 27,
        "divergent streaming exceeded client chunk bound",
    )?;
    require(
        app.world().resource::<VoxelWorld>().chunks.len() <= 270,
        "server loaded chunks did not remain bounded after traversal",
    )?;
    require(
        first.acknowledged > 900 && first.corrections > 0 && first.max_error.is_finite(),
        "loss/jitter reconciliation was not exercised",
    )?;
    require(
        first.state.position.distance(first.authority.position) < 0.25,
        "prediction failed to converge after jitter",
    )?;
    let old_id = first.id;
    let old_session = first.session;
    drop(first);
    drive(&mut app, &mut [&mut second], 12, [0.0; 2], 0.0)?;
    require(
        app.world().resource::<Simulation>().player_count() == 1,
        "disconnect leaked player",
    )?;
    let mut reconnected = Bot::connect(address, false)?;
    drive(
        &mut app,
        &mut [&mut reconnected, &mut second],
        180,
        [0.0; 2],
        -1.5,
    )?;
    require(
        reconnected.id != old_id && reconnected.session != old_session,
        "reconnect reused identity or session",
    )?;
    require(
        reconnected.world.chunks[&coord].revision == final_revision
            && reconnected.world.block(hit.block) == Some(0),
        "reconnect lost authoritative edit",
    )?;
    require(
        reconnected.world.chunks.len() <= 27,
        "reconnect interest exceeded bound",
    )?;
    app.world()
        .resource::<Simulation>()
        .print_metrics(app.world().resource::<VoxelWorld>().chunks.len());
    println!(
        "SMOKE PASS: two real TCP/UDP clients; reliable edits/revisions/rejection/resync; duplicate/loss/jitter input recovery; prediction reconciliation; cross-chunk streaming/forget; disconnect and fresh-session reconnect"
    );
    Ok(())
}
