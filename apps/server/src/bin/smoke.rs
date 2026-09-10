use bevy_app::App;
use glam::{IVec3, Vec3};
use networking::ClientTransport;
use physics::{EYE_HEIGHT, FIXED_DT, PlayerInput, PlayerState, step_player};
use protocol::{ArrowSnapshot, ClientMessage, EditRejection, Health, InputPacket, ServerMessage};
use simulation::{ServerConfig, Simulation, SimulationPlugin};
use std::{
    collections::{HashMap, HashSet, VecDeque},
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
    health: Option<Health>,
    remote_health: HashMap<u64, Health>,
    sequence: u64,
    acknowledged: u64,
    history: VecDeque<PlayerInput>,
    delayed: VecDeque<(u64, InputPacket)>,
    snapshot_tick: u64,
    remotes: usize,
    edits: HashMap<u64, bool>,
    damage: HashMap<u64, f32>,
    rejections: HashMap<u64, EditRejection>,
    flights: Vec<(u64, Vec<ArrowSnapshot>)>,
    explosions: Vec<(u32, Vec3, f32)>,
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
            health: None,
            remote_health: HashMap::new(),
            sequence: 0,
            acknowledged: 0,
            history: VecDeque::new(),
            delayed: VecDeque::new(),
            snapshot_tick: 0,
            remotes: 0,
            edits: HashMap::new(),
            damage: HashMap::new(),
            rejections: HashMap::new(),
            flights: Vec::new(),
            explosions: Vec::new(),
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
                    health,
                } => {
                    self.id = id;
                    self.session = session;
                    self.world.seed = seed;
                    self.state = spawn;
                    self.authority = spawn;
                    require(health == Health::default(), "welcome health was not full")?;
                    self.health = Some(health);
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
                ServerMessage::EditResult {
                    request,
                    accepted,
                    reason,
                    damage,
                } => {
                    self.edits.insert(request, accepted);
                    if let Some(damage) = damage {
                        self.damage.insert(request, damage);
                    }
                    if let Some(reason) = reason {
                        self.rejections.insert(request, reason);
                    }
                }
                ServerMessage::Physics { .. } => {}
                ServerMessage::Projectiles { tick, arrows } => self.flights.push((tick, arrows)),
                ServerMessage::Explosion {
                    id,
                    position,
                    radius,
                } => {
                    self.explosions.push((id, position, radius));
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
            self.health = Some(snapshot.you.health);
            self.remote_health = snapshot.players.iter().map(|p| (p.id, p.health)).collect();
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
/// Exercise repeated M1 contacts without treating their acknowledgements as deltas.
fn break_block(
    app: &mut App,
    bots: &mut [&mut Bot],
    target: IVec3,
    pitch: f32,
    request: &mut u64,
    replay_first: bool,
) -> Result<()> {
    let coord = chunk_coord(target);
    let revision = bots[0].world.chunks[&coord].revision;
    let material = bots[0].world.block(target);
    let deltas: Vec<_> = bots.iter().map(|bot| bot.deltas).collect();
    let mut per_hit = 0.0;
    for hit_index in 0..64 {
        *request += 1;
        let edit = ClientMessage::Edit {
            request: *request,
            target,
            block: 0,
            expected_revision: bots[0].world.chunks[&coord].revision,
        };
        bots[0].net.send(edit.clone())?;
        // Thirty fixed ticks exceed the successful-action cooldown and allow TCP
        // delivery before the next request reads the authoritative revision.
        drive(app, bots, 30, [0.0; 2], pitch)?;
        require(
            bots[0].edits.get(request) == Some(&true),
            &format!(
                "valid material hit {request} rejected: {:?}",
                bots[0].rejections.get(request)
            ),
        )?;
        let damage = *bots[0]
            .damage
            .get(request)
            .ok_or("accepted hit omitted damage")?;
        require(
            damage.is_finite() && damage > 0.0 && damage <= 1.0,
            "invalid fracture fraction",
        )?;
        if hit_index == 0 {
            per_hit = damage;
            require(damage < 1.0, "material was destroyed by a single M1 hit")?;
        }
        require(
            (damage - (per_hit * (hit_index + 1) as f32).min(1.0)).abs() < 0.0001,
            "fracture accumulation differed from accepted hits (possible replay damage)",
        )?;
        if damage == 1.0 {
            for (bot, before_deltas) in bots.iter().zip(&deltas) {
                require(
                    bot.world.block(target) == Some(0),
                    "destroyed block did not replicate",
                )?;
                require(
                    bot.world.chunks[&coord].revision == revision + 1,
                    "destruction revision mismatch",
                )?;
                require(
                    bot.deltas == before_deltas + 1,
                    "destruction did not replicate exactly once",
                )?;
            }
            return Ok(());
        }
        for (bot, before_deltas) in bots.iter().zip(&deltas) {
            require(
                bot.world.block(target) == material,
                "partial hit changed terrain",
            )?;
            require(
                bot.world.chunks[&coord].revision == revision,
                "partial hit changed chunk revision",
            )?;
            require(
                bot.deltas == *before_deltas,
                "partial hit emitted a terrain delta",
            )?;
        }
        if replay_first && hit_index == 0 {
            bots[0].edits.remove(request);
            bots[0].net.send(edit)?;
            drive(app, bots, 30, [0.0; 2], pitch)?;
            require(
                bots[0].edits.get(request) == Some(&true)
                    && bots[0].damage.get(request) == Some(&per_hit),
                "duplicate hit did not replay the original result",
            )?;
        }
    }
    Err("material did not break within 64 accepted hits".into())
}
/// A separate CPU-only world keeps blast damage out of the movement/edit scenarios.
fn explosive_bow() -> Result<()> {
    let plugin = SimulationPlugin::bind(ServerConfig {
        bind: "127.0.0.1:0".parse()?,
        radius: 1,
        metrics_every: 0,
        gpu_physics: false,
        ..Default::default()
    })?;
    let address = plugin.local_addr();
    let mut app = App::new();
    app.add_plugins(plugin);
    let mut first = Bot::connect(address, false)?;
    let mut second = Bot::connect(address, false)?;
    drive(&mut app, &mut [&mut first, &mut second], 180, [0.0; 2], 0.0)?;
    require(
        first.id != 0 && second.id != 0,
        "bow peers were not admitted",
    )?;
    require(first.authority.grounded, "bow shooter did not settle")?;
    let muzzle = first.authority.position + Vec3::Y * EYE_HEIGHT;
    let base = muzzle.floor().as_ivec3();
    let mut wall = Vec::new();
    let mut repairs = HashSet::new();
    // Author a clear twelve-metre lane and a stone impact wall before firing.
    // Preserve the ground under the shooter; resync both clients to this fixture.
    {
        let mut world = app.world_mut().resource_mut::<VoxelWorld>();
        for x in 1..=12 {
            for y in -1..=2 {
                for z in -1..=1 {
                    let target = base + IVec3::new(x, y, z);
                    require(world.block(target).is_some(), "bow fixture is not loaded")?;
                    let block = if x == 12 { 3 } else { 0 };
                    if world.set_block(target, block).is_some() {
                        repairs.insert(chunk_coord(target));
                    }
                    if x == 12 {
                        wall.push(target);
                    }
                }
            }
        }
    }
    for coord in repairs {
        for bot in [&mut first, &mut second] {
            bot.net.send(ClientMessage::Resync { coord })?;
        }
    }
    drive(&mut app, &mut [&mut first, &mut second], 60, [0.0; 2], 0.0)?;
    for bot in [&first, &second] {
        require(
            wall.iter().all(|&cell| bot.world.block(cell) == Some(3)),
            "bow wall baseline did not replicate",
        )?;
    }
    let yaw = -std::f32::consts::FRAC_PI_2;
    let shot = ClientMessage::FireBow {
        request: 1,
        yaw,
        pitch: 0.0,
    };
    first.net.send(shot.clone())?;
    first.net.send(shot.clone())?;
    first.net.send(ClientMessage::FireBow {
        request: 2,
        yaw,
        pitch: 0.0,
    })?;
    drive(&mut app, &mut [&mut first, &mut second], 120, [0.0; 2], 0.0)?;
    require(first.edits.get(&1) == Some(&true), "bow shot was rejected")?;
    require(
        first.edits.get(&2) == Some(&false)
            && first.rejections.get(&2) == Some(&EditRejection::Cooldown),
        "bow rapid-fire request was not rejected by cooldown",
    )?;
    require(
        first.explosions.len() == 1 && first.explosions == second.explosions,
        "bow impact did not replicate exactly once to both clients",
    )?;
    let (id, position, radius) = first.explosions[0];
    require(
        position.is_finite()
            && radius > 0.0
            && (position.x - (base.x + 12) as f32).abs() < 0.05
            && position.distance(muzzle) > 6.0,
        "bow did not impact the distant wall",
    )?;
    for bot in [&first, &second] {
        let visible: Vec<_> = bot
            .flights
            .iter()
            .flat_map(|(_, arrows)| arrows)
            .filter(|arrow| arrow.id == id)
            .collect();
        require(
            visible.len() >= 2
                && visible
                    .iter()
                    .all(|arrow| arrow.position.is_finite() && arrow.velocity.is_finite())
                && visible.last().unwrap().position.x > visible[0].position.x + 1.0,
            "bow moving arrow was not visible before impact",
        )?;
        require(
            bot.flights
                .iter()
                .all(|(_, arrows)| arrows.iter().all(|arrow| arrow.id == id))
                && bot
                    .flights
                    .last()
                    .is_some_and(|(_, arrows)| arrows.is_empty()),
            "bow replay spawned another arrow or impact left an active arrow",
        )?;
    }
    let destroyed: Vec<_> = wall
        .iter()
        .copied()
        .filter(|&cell| app.world().resource::<VoxelWorld>().block(cell) == Some(0))
        .collect();
    require(
        !destroyed.is_empty(),
        "bow impact caused no terrain destruction",
    )?;
    for bot in [&first, &second] {
        require(
            bot.deltas > 0
                && destroyed
                    .iter()
                    .all(|&cell| bot.world.block(cell) == Some(0)),
            "bow terrain destruction did not replicate to both clients",
        )?;
    }
    // Replay after the cooldown expires, requiring a fresh reply, not the old map entry.
    let deltas = [first.deltas, second.deltas];
    first.edits.remove(&1);
    first.edits.remove(&2);
    first.net.send(shot)?;
    first.net.send(ClientMessage::FireBow {
        request: 2,
        yaw,
        pitch: 0.0,
    })?;
    drive(&mut app, &mut [&mut first, &mut second], 210, [0.0; 2], 0.0)?;
    require(
        first.edits.get(&1) == Some(&true)
            && first.edits.get(&2) == Some(&false)
            && first.rejections.get(&2) == Some(&EditRejection::Cooldown),
        "bow replay did not preserve accepted and rejected results",
    )?;
    for (bot, before) in [&first, &second].into_iter().zip(deltas) {
        require(
            bot.explosions.len() == 1
                && bot.deltas == before
                && bot
                    .flights
                    .iter()
                    .all(|(_, arrows)| arrows.iter().all(|arrow| arrow.id == id)),
            "bow replay repeated flight, explosion, or terrain damage",
        )?;
    }
    first.net.send(ClientMessage::FireBow {
        request: 3,
        yaw,
        pitch: 0.5,
    })?;
    drive(&mut app, &mut [&mut first, &mut second], 210, [0.0; 2], 0.0)?;
    require(
        first.edits.get(&3) == Some(&true),
        "bow cooldown did not permit a later fresh shot",
    )?;
    for bot in [&first, &second] {
        require(
            bot.flights
                .iter()
                .any(|(_, arrows)| arrows.iter().any(|arrow| arrow.id != id)),
            "later bow shot did not replicate to both clients",
        )?;
    }
    Ok(())
}
fn check_health(app: &mut App, first: &mut Bot, second: &mut Bot) -> Result<()> {
    require(
        first.health == Some(Health::default()) && second.health == Some(Health::default()),
        "initial player health missing",
    )?;
    let applied = app
        .world_mut()
        .resource_mut::<Simulation>()
        .damage_player(first.id, 35);
    require(applied == Some(35), "server damage did not apply")?;
    drive(app, &mut [first, second], 30, [0.0; 2], -1.5)?;
    require(
        first.health.is_some_and(|h| h.current() == 65),
        "local health did not replicate",
    )?;
    require(
        second.remote_health.get(&first.id) == first.health.as_ref(),
        "remote health did not replicate",
    )?;
    require(
        second.health == Some(Health::default()),
        "damage changed the wrong player",
    )?;

    app.world_mut()
        .resource_mut::<Simulation>()
        .damage_player(first.id, u16::MAX);
    drive(app, &mut [first, second], 30, [0.0; 2], -1.5)?;
    require(
        first.health.is_some_and(Health::is_depleted),
        "depleted health did not replicate",
    )?;
    app.world_mut()
        .resource_mut::<Simulation>()
        .heal_player(first.id, u16::MAX);
    drive(app, &mut [first, second], 30, [0.0; 2], -1.5)?;
    require(
        first.health == Some(Health::default()),
        "healing did not replicate",
    )?;
    require(
        second.remote_health.get(&first.id) == first.health.as_ref(),
        "remote healing did not replicate",
    )?;
    println!("HEALTH_OK welcome=100 damage=65 depleted=0 healed=100 local_and_remote=true");
    Ok(())
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
    check_health(&mut app, &mut first, &mut second)?;
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
    let mut request = 0;
    break_block(
        &mut app,
        &mut [&mut first, &mut second],
        hit.block,
        -1.5,
        &mut request,
        true,
    )?;
    request += 1;
    first.net.send(ClientMessage::Edit {
        request,
        target: hit.block,
        block: 3,
        expected_revision: before,
    })?;
    drive(&mut app, &mut [&mut first, &mut second], 12, [0.0; 2], -1.5)?;
    require(
        first.edits.get(&request) == Some(&false)
            && first.rejections.get(&request) == Some(&EditRejection::StaleRevision),
        "stale edit revision was accepted",
    )?;
    request += 1;
    first.net.send(ClientMessage::Edit {
        request,
        target: hit.block + IVec3::X * 1000,
        block: 255,
        expected_revision: before + 1,
    })?;
    drive(&mut app, &mut [&mut first, &mut second], 12, [0.0; 2], -1.5)?;
    require(
        first.edits.get(&request) == Some(&false),
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
    request += 1;
    first.net.send(ClientMessage::Edit {
        request,
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
        first.edits.get(&request) == Some(&true),
        "valid placement was rejected",
    )?;
    require(
        first.world.block(placement) == Some(5) && second.world.block(placement) == Some(5),
        "placement did not replicate to both clients",
    )?;
    // Break the placed material through repeated authoritative contacts too.
    break_block(
        &mut app,
        &mut [&mut first, &mut second],
        placement,
        placement_pitch,
        &mut request,
        false,
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
    explosive_bow()?;
    println!(
        "SMOKE PASS: two real TCP/UDP clients; accumulating fracture/replay protection/destruction; reliable edits/revisions/rejection/resync; duplicate/loss/jitter input recovery; prediction reconciliation; cross-chunk streaming/forget; disconnect and fresh-session reconnect; CPU explosive bow flight/impact/destruction/cooldown/replay on both clients"
    );
    Ok(())
}
