use controller::PlayerInput;
mod bow_power_hud;
mod death_overlay;
mod drops;
mod game_hud;
mod health_hud;
mod lighting;
mod loose_blocks;
mod package_hud;
mod pause_menu;
mod projectiles;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    time::{Duration, Instant},
};

use bevy::{
    app::AppExit,
    input::mouse::AccumulatedMouseMotion,
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
    window::{CursorGrabMode, CursorOptions, PresentMode},
};
use networking::ClientTransport;
use physics::{EYE_HEIGHT, FIXED_DT, PLAYER_HEIGHT, PlayerState, look_direction};
use protocol::{
    BowPower, ClientMessage, EXPLOSIVE_BOW_SLOT, EditRejection, Health, InputPacket, Inventory,
    ServerMessage, Snapshot,
};
use voxel_render::{RenderFocus, VoxelRenderPlugin, VoxelRenderStats};
use voxel_world::{
    CHUNK_SIZE, Chunk, VoxelWorld, WORLD_MAX_Y, WORLD_MIN_Y, WorldPlugin, chunk_coord,
};

#[derive(Resource)]
struct Options {
    server: SocketAddr,
    bot: bool,
    frames: Option<u64>,
    screenshot: Option<String>,
    lighting: lighting::DayCycle,
}

impl Options {
    fn parse() -> Result<Self, String> {
        let mut options = Self {
            server: "127.0.0.1:4000".parse().unwrap(),
            bot: false,
            frames: None,
            screenshot: None,
            lighting: lighting::DayCycle::default(),
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--server" => {
                    options.server = args
                        .next()
                        .ok_or("--server needs IP:port")?
                        .parse()
                        .map_err(|_| "invalid server IP:port")?
                }
                "--bot" => options.bot = true,
                "--frames" => {
                    let frames = args
                        .next()
                        .ok_or("--frames needs a count")?
                        .parse::<u64>()
                        .map_err(|_| "invalid frame count")?;
                    if frames < 120 {
                        return Err(
                            "--frames must be at least 120 (allows startup and capture)".into()
                        );
                    }
                    options.frames = Some(frames);
                }
                "--screenshot" => {
                    options.screenshot = Some(args.next().ok_or("--screenshot needs a PNG path")?)
                }
                "--time-of-day" | "--day-length" => {
                    let value = args
                        .next()
                        .ok_or_else(|| format!("{arg} needs a number"))?
                        .parse::<f64>()
                        .map_err(|_| format!("invalid {arg}"))?;
                    if !value.is_finite()
                        || value < 0.0
                        || (arg == "--time-of-day" && value >= 24.0)
                    {
                        return Err(format!(
                            "{arg}: expected {}",
                            if arg == "--time-of-day" {
                                "hour in [0, 24)"
                            } else {
                                "nonnegative seconds (0 freezes time)"
                            }
                        ));
                    }
                    if arg == "--time-of-day" {
                        options.lighting.hour = value;
                    } else {
                        options.lighting.day_seconds = value;
                    }
                }
                "--help" | "-h" => {
                    println!(
                        "voxel-client [--server IP:PORT] [--bot] [--frames N] [--screenshot PATH.png] [--time-of-day HOUR] [--day-length SECONDS]\nLighting: starts at 09:00, 1200 seconds/day; --day-length 0 freezes time.\nWASD move | mouse look | Space jump/up | V noclip flight | Ctrl descend | left/right click hit/place (bow: hold right to shoot, R cycle power) | F debug launch (GPU server) | 1-5 material | 6 explosive bow | Esc pause menu | F12 screenshot"
                    );
                    std::process::exit(0);
                }
                _ => return Err(format!("unknown argument: {arg}")),
            }
        }
        Ok(options)
    }
}

#[derive(Resource)]
struct ClientSession {
    transport: Option<ClientTransport>,
    status: String,
    id: Option<u64>,
    session: u64,
    state: PlayerState,
    // Authoritative respawn generation; 0 until the first life change.
    life: u64,
    // Replicated independently of movement prediction.
    health: Health,
    inventory: Inventory,
    noclip_requested: bool,
    jump_pending: bool,
    attack_pending: bool,
    pending: VecDeque<PlayerInput>,
    sequence: u64,
    last_tick: u64,
    accumulator: f32,
    yaw: f32,
    pitch: f32,
    correction: Vec3,
    selected: u8,
    bow_power: BowPower,
    packages: package_hud::ServerPackages,
    request: u64,
    accepted_edits: u64,
    rejected_edits: u64,
    last_action: Option<ActionResult>,
    resyncing: HashSet<IVec3>,
    last_packet: Instant,
    started: Instant,
    frame: u64,
    captured: bool,
    frame_times: Vec<f64>,
    next_metrics: Instant,
    max_correction: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ActionResult {
    request: u64,
    accepted: bool,
    reason: Option<EditRejection>,
    damage: Option<f32>,
}

impl std::fmt::Display for ActionResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Action #{}: ", self.request)?;
        if !self.accepted {
            match self.reason {
                Some(reason) => write!(f, "rejected ({reason:?})"),
                None => write!(f, "rejected"),
            }
        } else if let Some(damage) = self.damage {
            write!(f, "fracture {:.0}%", damage * 100.0)
        } else {
            write!(f, "accepted")
        }
    }
}

impl Default for ClientSession {
    fn default() -> Self {
        Self {
            transport: None,
            status: "Connecting".into(),
            id: None,
            session: 0,
            state: PlayerState::default(),
            life: 0,
            health: Health::default(),
            inventory: Inventory::default(),
            noclip_requested: false,
            jump_pending: false,
            attack_pending: false,
            pending: VecDeque::with_capacity(256),
            sequence: 0,
            last_tick: 0,
            accumulator: 0.0,
            yaw: 0.0,
            pitch: -0.25,
            correction: Vec3::ZERO,
            selected: 3,
            bow_power: BowPower::default(),
            packages: package_hud::ServerPackages::default(),
            request: 0,
            accepted_edits: 0,
            rejected_edits: 0,
            last_action: None,
            resyncing: HashSet::new(),
            last_packet: Instant::now(),
            started: Instant::now(),
            frame: 0,
            captured: false,
            frame_times: Vec::with_capacity(600),
            next_metrics: Instant::now() + std::time::Duration::from_secs(10),
            max_correction: 0.0,
        }
    }
}

impl ClientSession {
    fn capture_jump(&mut self, enabled: bool, pressed: bool) {
        self.jump_pending = enabled && (self.jump_pending || pressed);
    }

    fn consume_jump(&mut self, flight: bool, held: bool) -> bool {
        let pressed = std::mem::take(&mut self.jump_pending);
        if flight { held } else { pressed }
    }
    /// One-tick melee request, consumed like jump so a click swings once.
    fn consume_attack(&mut self) -> bool {
        std::mem::take(&mut self.attack_pending)
    }
    fn predict_input(
        &mut self,
        world: &VoxelWorld,
        input: PlayerInput,
        bodies: &[physics::DynamicCollider],
    ) {
        controller::step_player_with_bodies(world, &mut self.state, &input, FIXED_DT, bodies);
        self.pending.push_back(input);
    }
    fn receive_action(&mut self, result: ActionResult) {
        if result.accepted {
            self.accepted_edits += 1;
        } else {
            self.rejected_edits += 1;
        }
        // Queued debug launches may complete after newer actions. Terrain changes
        // come only from Delta/Chunk messages, including for accepted partial hits.
        if self
            .last_action
            .is_none_or(|last| result.request >= last.request)
        {
            self.last_action = Some(result);
        }
    }
    fn disconnect(&mut self, reason: impl std::fmt::Display) {
        self.status = format!("Disconnected: {reason}");
        error!("{}", self.status);
        self.transport = None;
        self.pending.clear();
        self.packages = package_hud::ServerPackages::default();
    }

    fn send(&mut self, message: ClientMessage) {
        if let Some(transport) = &mut self.transport
            && let Err(error) = transport.send(message)
        {
            self.disconnect(error);
        }
    }

    fn resync(&mut self, coord: IVec3) {
        if self.resyncing.insert(coord) {
            self.send(ClientMessage::Resync { coord });
        }
    }
    /// Outgoing input packet tagged with the currently observed life counter.
    fn input_packet(&self, inputs: Vec<PlayerInput>) -> InputPacket {
        InputPacket {
            session: self.session,
            life: self.life,
            inputs,
        }
    }

    /// Keep the authenticated UDP endpoint alive without predicting dead movement.
    fn death_heartbeat(&mut self, dt: f32) -> Option<InputPacket> {
        self.accumulator += dt.min(0.1);
        if self.accumulator < 0.5 {
            return None;
        }
        self.accumulator = 0.0;
        self.sequence = self.sequence.max(1);
        Some(self.input_packet(vec![PlayerInput {
            sequence: self.sequence,
            ..Default::default()
        }]))
    }

    /// Respawn request for the currently observed life. Repeating it is harmless;
    /// the server ignores a stale or duplicate counter.
    fn respawn_request(&self) -> ClientMessage {
        ClientMessage::Respawn { life: self.life }
    }
}

struct RemotePlayer {
    entity: Entity,
    life: u64,
    health: Health,
    previous: Vec3,
    current: Vec3,
    previous_yaw: f32,
    yaw: f32,
    received: Instant,
}

struct RemoteActor {
    entity: Entity,
    material: Handle<StandardMaterial>,
    health: Health,
    previous: Vec3,
    current: Vec3,
    yaw: f32,
    received: Instant,
    flash_until: Instant,
}

#[derive(Resource, Default)]
struct RemoteActors(HashMap<u32, RemoteActor>);

#[derive(Resource, Default)]
struct RemotePlayers(HashMap<u64, RemotePlayer>);

#[derive(Resource)]
struct ActorAssets {
    mesh: Handle<Mesh>,
    material: Handle<StandardMaterial>,
    dummy_mesh: Handle<Mesh>,
}

#[derive(Component)]
struct PlayerCamera;
#[derive(Component)]
struct RemoteActorEntity;

#[derive(Component)]
struct Selection;

struct ClientPlugin;

impl Plugin for ClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(controller::ControllerPlugin::default());
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.add_systems(
            FixedUpdate,
            observe_character_bodies.in_set(controller::ControllerSet::Intent),
        );
        app.init_resource::<ClientSession>()
            .init_resource::<RemotePlayers>()
            .init_resource::<RemoteActors>()
            .init_resource::<pause_menu::PauseMenu>()
            .init_resource::<death_overlay::DeathOverlay>()
            .add_systems(Startup, setup)
            .add_systems(
                Update,
                (
                    receive_network,
                    pause_menu::input,
                    pause_menu::actions,
                    pause_menu::sync,
                    death_overlay::input,
                    death_overlay::actions,
                    death_overlay::sync,
                    controls,
                    bow_power_hud::cycle_on_click.run_if(pause_menu::gameplay_enabled),
                    predict,
                    edit_blocks,
                    present_players,
                    select_voxel,
                    game_hud::update,
                    game_hud::update_fps,
                    health_hud::update,
                    bow_power_hud::update,
                    package_hud::update,
                    capture_screenshot,
                    record_metrics,
                )
                    .chain(),
            );
    }
}

fn main() {
    let options = Options::parse().unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(2)
    });
    App::new()
        .insert_resource(options.lighting)
        .insert_resource(options)
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Voxel".into(),
                resolution: (1280, 800).into(),
                present_mode: PresentMode::AutoVsync,
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            bevy::diagnostic::FrameTimeDiagnosticsPlugin::default(),
            WorldPlugin,
            VoxelRenderPlugin,
            ClientPlugin,
            lighting::LightingPlugin,
            loose_blocks::LooseBlocksPlugin,
            projectiles::ProjectilesPlugin,
            drops::DropsPlugin,
        ))
        .run();
}

/// Camera far plane covering the largest server interest square in three
/// dimensions: the horizontal corner distance plus the full vertical span of
/// the world, so far mountain terrain is not clipped when its elevation is far
/// from the camera.
fn camera_far_distance() -> f32 {
    let horizontal =
        (protocol::MAX_VIEW_RADIUS + 2) as f32 * CHUNK_SIZE as f32 * std::f32::consts::SQRT_2;
    let vertical = (WORLD_MAX_Y - WORLD_MIN_Y + 1) as f32 + EYE_HEIGHT;
    (horizontal * horizontal + vertical * vertical).sqrt()
}

fn setup(
    mut commands: Commands,
    options: Res<Options>,
    mut session: ResMut<ClientSession>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut cursor: Single<&mut CursorOptions>,
) {
    match ClientTransport::connect(options.server) {
        Ok(transport) => {
            session.transport = Some(transport);
            session.status = format!("Joining {}", options.server);
        }
        Err(error) => session.disconnect(error),
    }
    cursor.grab_mode = if options.bot {
        CursorGrabMode::None
    } else {
        CursorGrabMode::Locked
    };
    cursor.visible = options.bot;
    commands.spawn((
        Camera3d::default(),
        Projection::Perspective(PerspectiveProjection {
            fov: 75.0_f32.to_radians(),
            // Cover the 3D diagonal of the largest server interest square,
            // including the full vertical extent of the voxel world.
            far: camera_far_distance(),
            ..default()
        }),
        Transform::from_translation(session.state.position + Vec3::Y * EYE_HEIGHT),
        PlayerCamera,
    ));
    commands.insert_resource(ActorAssets {
        mesh: meshes.add(Cuboid::new(0.6, PLAYER_HEIGHT, 0.6)),
        material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.2, 0.55, 0.95),
            perceptual_roughness: 1.0,
            ..default()
        }),
        dummy_mesh: meshes.add(Capsule3d::new(0.32, 1.1)),
    });
    // A translucent shell marks the selected solid voxel without requiring wireframe support.
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::from_length(1.006))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgba(1.0, 0.87, 0.3, 0.16),
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            ..default()
        })),
        Transform::default(),
        Visibility::Hidden,
        Selection,
    ));
    game_hud::spawn(&mut commands);
    pause_menu::spawn(&mut commands);
    death_overlay::spawn(&mut commands);
    health_hud::spawn(&mut commands);
    bow_power_hud::spawn(&mut commands);
    package_hud::spawn(&mut commands);
}

#[allow(clippy::too_many_arguments)] // Independent Bevy presentation resources.
fn receive_network(
    mut commands: Commands,
    mut session: ResMut<ClientSession>,
    mut world: ResMut<VoxelWorld>,
    mut remotes: ResMut<RemotePlayers>,
    mut actors: ResMut<RemoteActors>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    assets: Res<ActorAssets>,
    mut loose: ResMut<loose_blocks::LooseBlocks>,
    loose_assets: Res<loose_blocks::LooseBlockAssets>,
    mut projectiles: ResMut<projectiles::Projectiles>,
    projectile_assets: Res<projectiles::ProjectileAssets>,
    mut drops: ResMut<drops::Drops>,
    drop_assets: Res<drops::DropAssets>,
) {
    let Some(transport) = &mut session.transport else {
        return;
    };
    let incoming = match transport.poll() {
        Ok(incoming) => incoming,
        Err(error) => {
            session.disconnect(error);
            return;
        }
    };
    if !incoming.reliable.is_empty() || !incoming.snapshots.is_empty() {
        session.last_packet = Instant::now();
    }
    for message in incoming.reliable {
        match message {
            ServerMessage::Welcome {
                id,
                session: token,
                seed,
                spawn,
                health,
                inventory,
            } => {
                projectiles.clear(&mut commands);
                drops.clear(&mut commands);
                session.packages = package_hud::ServerPackages::default();
                session.id = Some(id);
                session.session = token;
                session.state = spawn;
                session.noclip_requested = spawn.noclip;
                session.jump_pending = false;
                session.health = health;
                session.inventory = inventory;
                session.life = 0;
                session.pending.clear();
                session.correction = Vec3::ZERO;
                session.accumulator = 0.0;
                world.seed = seed;
                session.status = format!("Connected | player {id}");
                info!("WELCOME player={id} seed={seed}");
            }
            ServerMessage::Chunk {
                coord,
                revision,
                runs,
            } => match Chunk::from_runs(revision, &runs) {
                Ok(chunk) => {
                    world.insert(coord, chunk);
                    session.resyncing.remove(&coord);
                }
                Err(error) => {
                    session.disconnect(format!("invalid chunk: {error}"));
                    break;
                }
            },
            ServerMessage::Delta {
                coord,
                from,
                to,
                local_index,
                block,
            } => {
                let valid = world
                    .chunks
                    .get(&coord)
                    .is_some_and(|chunk| chunk.revision == from)
                    && to == from.saturating_add(1)
                    && usize::from(local_index) < voxel_world::CHUNK_VOLUME
                    && block <= 5;
                if !valid {
                    session.resync(coord);
                    continue;
                }
                let index = i32::from(local_index);
                let local = IVec3::new(
                    index % CHUNK_SIZE,
                    index / (CHUNK_SIZE * CHUNK_SIZE),
                    (index / CHUNK_SIZE) % CHUNK_SIZE,
                );
                if world.set_block(coord * CHUNK_SIZE + local, block) != Some((from, to)) {
                    session.resync(coord);
                }
            }
            ServerMessage::Forget { coord } => {
                world.remove(coord);
                session.resyncing.remove(&coord);
            }
            ServerMessage::EditResult {
                request,
                accepted,
                reason,
                damage,
            } => {
                session.receive_action(ActionResult {
                    request,
                    accepted,
                    reason,
                    damage,
                });
                info!(
                    "EDIT_RESULT request={request} accepted={accepted} reason={reason:?} damage={damage:?}"
                );
            }
            ServerMessage::Physics { tick, bodies } => {
                loose.receive(tick, &bodies, &mut commands, &loose_assets);
            }
            ServerMessage::Projectiles { tick, arrows } => {
                projectiles.receive(
                    tick,
                    &arrows,
                    &mut commands,
                    &projectile_assets,
                    Instant::now(),
                );
            }
            ServerMessage::Explosion {
                id,
                position,
                radius,
            } => {
                projectiles.explode(
                    id,
                    position,
                    radius,
                    &mut commands,
                    &projectile_assets,
                    Instant::now(),
                );
            }
            ServerMessage::Packages {
                revision,
                packages,
                bow_shots_per_second,
            } => {
                session
                    .packages
                    .receive(revision, packages, bow_shots_per_second);
            }
            ServerMessage::Inventory { inventory } => {
                session.inventory = inventory;
            }
            ServerMessage::Drops {
                tick,
                drops: snapshots,
            } => {
                drops.receive(
                    tick,
                    &snapshots,
                    &mut commands,
                    &drop_assets,
                    Instant::now(),
                );
            }
            ServerMessage::Disconnect { reason } => {
                session.disconnect(reason);
                break;
            }
        }
    }
    for snapshot in incoming.snapshots {
        if snapshot.tick <= session.last_tick || Some(snapshot.you.id) != session.id {
            continue;
        }
        reconcile(&mut session, &world, &snapshot, loose.colliders());
        let now = Instant::now();
        remotes.0.retain(|id, remote| {
            if snapshot.players.iter().any(|player| player.id == *id) {
                true
            } else {
                commands.entity(remote.entity).despawn();
                false
            }
        });
        for player in snapshot.players {
            remotes
                .0
                .entry(player.id)
                .and_modify(|remote| {
                    remote.previous = remote.current;
                    remote.previous_yaw = remote.yaw;
                    remote.current = player.state.position;
                    remote.yaw = player.yaw;
                    remote.health = player.health;
                    if remote.life != player.life {
                        remote.previous = remote.current;
                        remote.previous_yaw = remote.yaw;
                        remote.life = player.life;
                    }
                    remote.received = now;
                })
                .or_insert_with(|| RemotePlayer {
                    entity: commands
                        .spawn((
                            Mesh3d(assets.mesh.clone()),
                            MeshMaterial3d(assets.material.clone()),
                            Transform::from_translation(
                                player.state.position + Vec3::Y * PLAYER_HEIGHT * 0.5,
                            ),
                        ))
                        .id(),
                    previous: player.state.position,
                    life: player.life,
                    health: player.health,
                    current: player.state.position,
                    previous_yaw: player.yaw,
                    yaw: player.yaw,
                    received: now,
                });
        }
        actors.0.retain(|id, actor| {
            if snapshot.actors.iter().any(|a| a.id == *id) {
                true
            } else {
                commands.entity(actor.entity).despawn();
                false
            }
        });
        for actor in snapshot.actors {
            actors
                .0
                .entry(actor.id)
                .and_modify(|remote| {
                    remote.previous = remote.current;
                    remote.current = actor.state.position;
                    remote.yaw = actor.yaw;
                    if actor.health.current() < remote.health.current() {
                        remote.flash_until = now + Duration::from_millis(120);
                    }
                    remote.health = actor.health;
                    remote.received = now;
                })
                .or_insert_with(|| {
                    let material = materials.add(StandardMaterial {
                        base_color: Color::srgb(0.85, 0.45, 0.25),
                        perceptual_roughness: 0.8,
                        ..default()
                    });
                    RemoteActor {
                        entity: commands
                            .spawn((
                                Mesh3d(assets.dummy_mesh.clone()),
                                MeshMaterial3d(material.clone()),
                                Transform::from_translation(
                                    actor.state.position + Vec3::Y * PLAYER_HEIGHT * 0.5,
                                ),
                                RemoteActorEntity,
                            ))
                            .id(),
                        material,
                        health: actor.health,
                        previous: actor.state.position,
                        current: actor.state.position,
                        yaw: actor.yaw,
                        received: now,
                        flash_until: now,
                    }
                });
        }
    }
    if session.last_packet.elapsed().as_secs() > 10 {
        session.disconnect("server timed out");
    }
}

fn reconcile(
    session: &mut ClientSession,
    world: &VoxelWorld,
    snapshot: &Snapshot,
    bodies: &[physics::DynamicCollider],
) {
    if snapshot.tick <= session.last_tick {
        // Out-of-order or duplicate datagram: keep the newer authoritative state.
        return;
    }
    session.last_tick = snapshot.tick;
    let previous = session.state.position;
    let life_changed = snapshot.you.life != session.life;
    session.life = snapshot.you.life;
    session.health = snapshot.you.health;
    session.state = snapshot.you.state;
    while session
        .pending
        .front()
        .is_some_and(|input| input.sequence <= snapshot.you.last_input)
    {
        session.pending.pop_front();
    }
    if life_changed {
        // Authoritative respawn: drop replay from the previous life and adopt the
        // server's motion, noclip request and presentation. The input sequence
        // stays monotonic so later commands remain acceptable to the server.
        session.pending.clear();
        session.correction = Vec3::ZERO;
        session.accumulator = 0.0;
        session.noclip_requested = snapshot.you.state.noclip;
        session.jump_pending = false;
        return;
    }
    if session.health.is_depleted() {
        // Dead players have no local movement to replay.
        session.pending.clear();
        session.correction = Vec3::ZERO;
        return;
    }
    for input in &session.pending {
        controller::step_player_with_bodies(world, &mut session.state, input, FIXED_DT, bodies);
    }
    let difference = previous - session.state.position;
    session.max_correction = session.max_correction.max(difference.length());
    if difference.length_squared() < 4.0 {
        session.correction = (session.correction + difference).clamp_length_max(2.0);
    } else {
        session.correction = Vec3::ZERO;
    }
}

fn controls(
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<AccumulatedMouseMotion>,
    options: Res<Options>,
    mut session: ResMut<ClientSession>,
    cursor: Single<&CursorOptions>,
    menu: Res<pause_menu::PauseMenu>,
) {
    if menu.blocks_gameplay() || session.health.is_depleted() {
        return;
    }
    if !cursor.visible && !options.bot {
        session.yaw = (session.yaw - mouse.delta.x * 0.0025) % std::f32::consts::TAU;
        session.pitch = (session.pitch - mouse.delta.y * 0.0025).clamp(-1.54, 1.54);
    }
    session.selected = selected_slot(&keys).unwrap_or(session.selected);
    if !options.bot
        && !cursor.visible
        && session.selected == EXPLOSIVE_BOW_SLOT
        && keys.just_pressed(KeyCode::KeyR)
    {
        session.bow_power = session.bow_power.next();
    }
}

fn selected_slot(keys: &ButtonInput<KeyCode>) -> Option<u8> {
    [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
        KeyCode::Digit0,
    ]
    .into_iter()
    .enumerate()
    .filter(|(_, key)| keys.just_pressed(*key))
    .map(|(index, _)| index as u8 + 1)
    .next_back()
}
#[allow(clippy::too_many_arguments)] // Prediction reads input, pause state and world observations.
fn predict(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    cursor: Single<&CursorOptions>,
    options: Res<Options>,
    world: Res<VoxelWorld>,
    mut session: ResMut<ClientSession>,
    loose: Res<loose_blocks::LooseBlocks>,
    menu: Res<pause_menu::PauseMenu>,
) {
    if session.id.is_none() || session.transport.is_none() {
        return;
    }
    if session.health.is_depleted() {
        session.jump_pending = false;
        if let Some(packet) = session.death_heartbeat(time.delta_secs())
            && let Some(transport) = &mut session.transport
            && let Err(error) = transport.send_inputs(packet)
        {
            session.disconnect(error);
        }
        return;
    }
    if !menu.blocks_gameplay()
        && !options.bot
        && !cursor.visible
        && keys.just_pressed(KeyCode::KeyV)
    {
        session.noclip_requested = !session.noclip_requested;
    }
    if !session.noclip_requested
        && !session.state.noclip
        && world
            .block(session.state.position.floor().as_ivec3())
            .is_none()
    {
        session.accumulator = 0.0;
        return;
    }
    session.capture_jump(
        !options.bot && !cursor.visible && !menu.blocks_gameplay(),
        keys.just_pressed(KeyCode::Space),
    );
    session.accumulator += time.delta_secs().min(0.1);
    while session.accumulator >= FIXED_DT {
        session.accumulator -= FIXED_DT;
        // Stop producing new commands when the acknowledgement window is full.
        // Continue retransmission below so this recovers once the link returns.
        if session.pending.len() >= 256 {
            continue;
        }
        session.sequence += 1;
        let mut movement = [0.0, 0.0];
        let mut jump = false;
        let mut descend = false;
        if options.bot && !menu.blocks_gameplay() {
            // Reproducible traversal for profiling the real rendered client.
            let phase = (session.sequence / 240) % 4;
            movement = match phase {
                0 => [0.0, 1.0],
                1 => [1.0, 0.0],
                2 => [0.0, -1.0],
                _ => [-1.0, 0.0],
            };
            jump = session.sequence.is_multiple_of(90);
        } else if !cursor.visible && !menu.blocks_gameplay() {
            movement[0] = f32::from(u8::from(keys.pressed(KeyCode::KeyD)))
                - f32::from(u8::from(keys.pressed(KeyCode::KeyA)));
            movement[1] = f32::from(u8::from(keys.pressed(KeyCode::KeyW)))
                - f32::from(u8::from(keys.pressed(KeyCode::KeyS)));
            let flight = session.noclip_requested || session.state.noclip;
            jump = session.consume_jump(flight, keys.pressed(KeyCode::Space));
            descend = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
        }
        let input = PlayerInput {
            sequence: session.sequence,
            movement,
            yaw: session.yaw,
            pitch: session.pitch,
            jump,
            descend,
            noclip: session.noclip_requested,
            attack: session.consume_attack(),
            selected: session.selected,
        };
        session.predict_input(&world, input, loose.colliders());
    }
    if !session.pending.is_empty() {
        let inputs = session
            .pending
            .iter()
            .skip(session.pending.len().saturating_sub(8))
            .copied()
            .collect();
        let packet = session.input_packet(inputs);
        if let Some(transport) = &mut session.transport
            && let Err(error) = transport.send_inputs(packet)
        {
            session.disconnect(error);
        }
    }
}

#[allow(clippy::too_many_arguments)] // Gate gameplay actions separately from menu interaction.
fn edit_blocks(
    buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    cursor: Single<&CursorOptions>,
    world: Res<VoxelWorld>,
    mut session: ResMut<ClientSession>,
    actors: Res<RemoteActors>,
    remotes: Res<RemotePlayers>,
    time: Res<Time>,
    mut bow_repeat: Local<BowRepeat>,
    menu: Res<pause_menu::PauseMenu>,
) {
    if menu.blocks_gameplay()
        || cursor.visible
        || session.transport.is_none()
        || session.id.is_none()
        || session.health.is_depleted()
    {
        *bow_repeat = BowRepeat::default();
        return;
    }
    let strike = keys.just_pressed(KeyCode::KeyF);
    // Melee: a living actor or remote player under the crosshair within reach
    // swings instead of mining. The server re-validates; this only routes input.
    let origin = session.state.position + Vec3::Y * EYE_HEIGHT;
    let direction = look_direction(session.yaw, session.pitch);
    let melee = buttons.pressed(MouseButton::Left)
        && melee_target(&world, origin, direction, &actors, &remotes, session.selected);
    if melee {
        session.attack_pending = true;
    }
    let hit = buttons.just_pressed(MouseButton::Left) && !melee;
    let bow_shot = repeat_bow(
        &mut bow_repeat,
        time.elapsed_secs_f64(),
        session.selected == EXPLOSIVE_BOW_SLOT
            && buttons.pressed(MouseButton::Right)
            && !strike
            && !hit,
        session.packages.bow_shots_per_second,
    );
    let secondary = if session.selected == EXPLOSIVE_BOW_SLOT {
        bow_shot
    } else {
        buttons.just_pressed(MouseButton::Right)
    };
    if let Some(message) = block_action(&mut session, &world, strike, hit, secondary) {
        session.send(message);
    }
}

/// True when a living actor or remote player is under the crosshair within
/// melee range and no terrain blocks the swing. Client-side routing only.
fn melee_target(
    world: &VoxelWorld,
    origin: Vec3,
    direction: Vec3,
    actors: &RemoteActors,
    remotes: &RemotePlayers,
    selected: u8,
) -> bool {
    let spec = gameplay::combat::melee_spec(gameplay::combat::slot_item(selected));
    let wall = world
        .raycast(origin, direction, spec.range)
        .map(|hit| hit.distance)
        .unwrap_or(spec.range);
    let shape = physics::CollisionShape::default();
    actors
        .0
        .values()
        .filter(|actor| !actor.health.is_depleted())
        .any(|actor| {
            physics::raycast_body(origin, direction, actor.current, shape)
                .is_some_and(|distance| distance < wall)
        })
        || remotes
            .0
            .values()
            .filter(|remote| !remote.health.is_depleted())
            .any(|remote| {
                physics::raycast_body(origin, direction, remote.current, shape)
                    .is_some_and(|distance| distance < wall)
            })
}

#[derive(Default)]
struct BowRepeat {
    next: Option<f64>,
    rate: u32,
}

/// Repeat while held, preserving fractional-frame cadence without catch-up bursts.
fn repeat_bow(repeat: &mut BowRepeat, now: f64, held: bool, rate: u32) -> bool {
    if repeat.rate != rate {
        repeat.next = None;
        repeat.rate = rate;
    }
    if !held || rate == 0 {
        repeat.next = None;
        return false;
    }
    let interval = 1.0 / f64::from(rate);
    let deadline = repeat.next.unwrap_or(now);
    if now + 1e-9 < deadline {
        return false;
    }
    repeat.next = Some(if now - deadline < interval {
        deadline + interval
    } else {
        now + interval
    });
    true
}

fn block_action(
    session: &mut ClientSession,
    world: &VoxelWorld,
    strike: bool,
    hit: bool,
    secondary: bool,
) -> Option<ClientMessage> {
    // Bow shots do not need a nearby grid target. The server owns the arrow origin.
    if !strike && !hit && secondary && session.selected == EXPLOSIVE_BOW_SLOT {
        session.request += 1;
        return Some(ClientMessage::FireBow {
            request: session.request,
            yaw: session.yaw,
            pitch: session.pitch,
            power: session.bow_power,
        });
    }
    let block = if strike || hit {
        0
    } else if secondary {
        // Only placeable blocks (items 1-6) place; tools swing instead.
        let item = gameplay::combat::slot_item(session.selected);
        if !(1..=voxel_world::BEDROLL).contains(&item) || session.inventory.count(item) == 0 {
            return None;
        }
        item
    } else {
        return None;
    };
    let origin = session.state.position + Vec3::Y * EYE_HEIGHT;
    let hit = world.raycast(origin, look_direction(session.yaw, session.pitch), 6.0)?;
    let target = if block == 0 { hit.block } else { hit.adjacent };
    let expected_revision = world.chunks.get(&chunk_coord(target))?.revision;
    session.request += 1;
    let request = session.request;
    Some(if strike {
        ClientMessage::Strike {
            request,
            target,
            expected_revision,
        }
    } else {
        ClientMessage::Edit {
            request,
            target,
            block,
            expected_revision,
        }
    })
}

fn present_players(
    time: Res<Time>,
    mut session: ResMut<ClientSession>,
    remotes: Res<RemotePlayers>,
    actors: Res<RemoteActors>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut focus: ResMut<RenderFocus>,
    mut camera: Single<&mut Transform, With<PlayerCamera>>,
    mut transforms: Query<&mut Transform, Without<PlayerCamera>>,
    mut visibility: Query<&mut Visibility, With<RemoteActorEntity>>,
) {
    session.correction *= (-18.0 * time.delta_secs()).exp();
    camera.translation = session.state.position + Vec3::Y * EYE_HEIGHT + session.correction;
    camera.rotation = Quat::from_rotation_y(session.yaw) * Quat::from_rotation_x(session.pitch);
    focus.0 = session.state.position;
    for remote in remotes.0.values() {
        if let Ok(mut transform) = transforms.get_mut(remote.entity) {
            let alpha = (remote.received.elapsed().as_secs_f32() / 0.05).clamp(0.0, 1.0);
            transform.translation =
                remote.previous.lerp(remote.current, alpha) + Vec3::Y * PLAYER_HEIGHT * 0.5;
            transform.rotation = Quat::from_rotation_y(remote.previous_yaw)
                .slerp(Quat::from_rotation_y(remote.yaw), alpha);
        }
    }
    for actor in actors.0.values() {
        if let Ok(mut transform) = transforms.get_mut(actor.entity) {
            let alpha = (actor.received.elapsed().as_secs_f32() / 0.05).clamp(0.0, 1.0);
            transform.translation =
                actor.previous.lerp(actor.current, alpha) + Vec3::Y * PLAYER_HEIGHT * 0.5;
            transform.rotation = Quat::from_rotation_y(actor.yaw);
        }
        if let Ok(mut visible) = visibility.get_mut(actor.entity) {
            *visible = if actor.health.is_depleted() {
                Visibility::Hidden
            } else {
                Visibility::Visible
            };
        }
        if let Some(material) = materials.get_mut(&actor.material) {
            material.base_color = if Instant::now() < actor.flash_until {
                Color::srgb(1.0, 0.9, 0.7)
            } else {
                Color::srgb(0.85, 0.45, 0.25)
            };
        }
    }
}

fn select_voxel(
    world: Res<VoxelWorld>,
    session: Res<ClientSession>,
    menu: Res<pause_menu::PauseMenu>,
    mut selection: Single<(&mut Transform, &mut Visibility), With<Selection>>,
) {
    if menu.blocks_gameplay() || session.health.is_depleted() {
        *selection.1 = Visibility::Hidden;
        return;
    }
    let origin = session.state.position + Vec3::Y * EYE_HEIGHT;
    if let Some(hit) = world.raycast(origin, look_direction(session.yaw, session.pitch), 6.0) {
        selection.0.translation = hit.block.as_vec3() + Vec3::splat(0.5);
        *selection.1 = Visibility::Visible;
    } else {
        *selection.1 = Visibility::Hidden;
    }
}

fn capture_screenshot(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    options: Res<Options>,
    mut session: ResMut<ClientSession>,
) {
    let auto_capture = !session.captured
        && options.screenshot.is_some()
        && session.frame >= options.frames.unwrap_or(900).saturating_sub(60);
    if auto_capture || keys.just_pressed(KeyCode::F12) {
        let path = options
            .screenshot
            .clone()
            .unwrap_or_else(|| format!("/tmp/voxel-{}.png", session.frame));
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path.clone()));
        session.captured = true;
        info!("CAPTURE {path}");
    }
}

fn record_metrics(
    time: Res<Time>,
    options: Res<Options>,
    world: Res<VoxelWorld>,
    stats: Res<VoxelRenderStats>,
    mut session: ResMut<ClientSession>,
    mut exit: MessageWriter<AppExit>,
) {
    session.frame += 1;
    if session.frame > 60 {
        session.frame_times.push(time.delta_secs_f64() * 1000.0);
    }
    let ending = options.frames.is_some_and(|frames| session.frame >= frames);
    if ending || Instant::now() >= session.next_metrics {
        session.frame_times.sort_unstable_by(f64::total_cmp);
        let percentile = |fraction: f64| -> f64 {
            if session.frame_times.is_empty() {
                0.0
            } else {
                session.frame_times[((session.frame_times.len() - 1) as f64 * fraction) as usize]
            }
        };
        info!(
            "CLIENT_METRICS elapsed_s={:.1} frames={} p50_ms={:.2} p95_ms={:.2} p99_ms={:.2} chunks={} meshes={} triangles={} jobs={} upload_bytes={} stale_jobs={} pending_inputs={} max_correction={:.3}",
            session.started.elapsed().as_secs_f64(),
            session.frame,
            percentile(0.5),
            percentile(0.95),
            percentile(0.99),
            world.chunks.len(),
            stats.visible_chunks,
            stats.triangles,
            stats.pending_jobs,
            stats.uploaded_bytes,
            stats.stale_jobs,
            session.pending.len(),
            session.max_correction
        );
        session.frame_times.clear();
        session.next_metrics = Instant::now() + std::time::Duration::from_secs(10);
    }
    if ending {
        exit.write(AppExit::Success);
    }
}

#[cfg(test)]
mod tests;

fn observe_character_bodies(
    loose: Res<loose_blocks::LooseBlocks>,
    mut bodies: ResMut<controller::ObservedBodies>,
) {
    bodies.0.clear();
    bodies.0.extend_from_slice(loose.colliders());
}
