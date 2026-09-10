mod loose_blocks;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    time::Instant,
};

use bevy::{
    app::AppExit,
    diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin},
    input::mouse::AccumulatedMouseMotion,
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
    window::{CursorGrabMode, CursorOptions, PresentMode},
};
use networking::ClientTransport;
use physics::{EYE_HEIGHT, FIXED_DT, PLAYER_HEIGHT, PlayerInput, PlayerState, look_direction};
use protocol::{ClientMessage, EditRejection, InputPacket, ServerMessage, Snapshot};
use voxel_render::{RenderFocus, VoxelRenderPlugin, VoxelRenderStats};
use voxel_world::{CHUNK_SIZE, Chunk, VoxelWorld, WorldPlugin, chunk_coord};

#[derive(Resource)]
struct Options {
    server: SocketAddr,
    bot: bool,
    frames: Option<u64>,
    screenshot: Option<String>,
}

impl Options {
    fn parse() -> Result<Self, String> {
        let mut options = Self {
            server: "127.0.0.1:4000".parse().unwrap(),
            bot: false,
            frames: None,
            screenshot: None,
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
                "--help" | "-h" => {
                    println!(
                        "voxel-client [--server IP:PORT] [--bot] [--frames N] [--screenshot PATH.png]\nWASD move | mouse look | Space jump | left/right click hit/place | F debug launch (GPU server) | 1-5 material | Esc release/capture mouse | F12 screenshot"
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
    pending: VecDeque<PlayerInput>,
    sequence: u64,
    last_tick: u64,
    accumulator: f32,
    yaw: f32,
    pitch: f32,
    correction: Vec3,
    selected: u8,
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
            pending: VecDeque::with_capacity(256),
            sequence: 0,
            last_tick: 0,
            accumulator: 0.0,
            yaw: 0.0,
            pitch: -0.25,
            correction: Vec3::ZERO,
            selected: 3,
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
}

struct RemotePlayer {
    entity: Entity,
    previous: Vec3,
    current: Vec3,
    previous_yaw: f32,
    yaw: f32,
    received: Instant,
}

#[derive(Resource, Default)]
struct RemotePlayers(HashMap<u64, RemotePlayer>);

#[derive(Resource)]
struct ActorAssets {
    mesh: Handle<Mesh>,
    material: Handle<StandardMaterial>,
}

#[derive(Component)]
struct PlayerCamera;
#[derive(Component)]
struct Hud;
#[derive(Component)]
struct Selection;

struct ClientPlugin;

impl Plugin for ClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ClientSession>()
            .init_resource::<RemotePlayers>()
            .add_systems(Startup, setup)
            .add_systems(
                Update,
                (
                    receive_network,
                    controls,
                    predict,
                    edit_blocks,
                    present_players,
                    select_voxel,
                    update_hud,
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
        .insert_resource(options)
        .insert_resource(ClearColor(Color::srgb(0.48, 0.69, 0.88)))
        .insert_resource(AmbientLight {
            color: Color::WHITE,
            brightness: 500.0,
            ..default()
        })
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Voxel / multiplayer laboratory".into(),
                resolution: (1280, 800).into(),
                present_mode: PresentMode::AutoVsync,
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            FrameTimeDiagnosticsPlugin::default(),
            WorldPlugin,
            VoxelRenderPlugin,
            ClientPlugin,
            loose_blocks::LooseBlocksPlugin,
        ))
        .run();
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
            far: 384.0,
            ..default()
        }),
        Transform::from_translation(session.state.position + Vec3::Y * EYE_HEIGHT),
        PlayerCamera,
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadows_enabled: false,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.9, -0.7, 0.0)),
    ));
    commands.insert_resource(ActorAssets {
        mesh: meshes.add(Cuboid::new(0.6, PLAYER_HEIGHT, 0.6)),
        material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.2, 0.55, 0.95),
            perceptual_roughness: 1.0,
            ..default()
        }),
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
    commands.spawn((
        Text::new("VOXEL / CONNECTING"),
        TextFont {
            font_size: 17.0,
            ..default()
        },
        TextColor(Color::srgb(0.94, 0.97, 1.0)),
        TextShadow::default(),
        Node {
            position_type: PositionType::Absolute,
            top: px(18),
            left: px(20),
            padding: UiRect::all(px(12)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.025, 0.04, 0.07, 0.8)),
        Hud,
    ));
    commands.spawn((
        Text::new("+"),
        TextFont {
            font_size: 24.0,
            ..default()
        },
        TextShadow::default(),
        Node {
            position_type: PositionType::Absolute,
            left: percent(50),
            top: percent(50),
            margin: UiRect {
                left: px(-7),
                top: px(-14),
                ..default()
            },
            ..default()
        },
    ));
    commands.spawn((
        Text::new("WASD  MOVE    SPACE  JUMP    MOUSE  LOOK\nLMB  HIT    RMB  PLACE    F  DEBUG LAUNCH    1-5  MATERIAL    ESC  CURSOR    F12  CAPTURE"),
        TextFont { font_size: 14.0, ..default() }, TextShadow::default(),
        Node { position_type: PositionType::Absolute, bottom: px(18), left: px(20), padding: UiRect::all(px(10)), ..default() },
        BackgroundColor(Color::srgba(0.025, 0.04, 0.07, 0.8)),
    ));
}

fn receive_network(
    mut commands: Commands,
    mut session: ResMut<ClientSession>,
    mut world: ResMut<VoxelWorld>,
    mut remotes: ResMut<RemotePlayers>,
    assets: Res<ActorAssets>,
    mut loose: ResMut<loose_blocks::LooseBlocks>,
    loose_assets: Res<loose_blocks::LooseBlockAssets>,
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
            } => {
                session.id = Some(id);
                session.session = token;
                session.state = spawn;
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
                    current: player.state.position,
                    previous_yaw: player.yaw,
                    yaw: player.yaw,
                    received: now,
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
    session.last_tick = snapshot.tick;
    while session
        .pending
        .front()
        .is_some_and(|input| input.sequence <= snapshot.you.last_input)
    {
        session.pending.pop_front();
    }
    let previous = session.state.position;
    session.state = snapshot.you.state;
    for input in &session.pending {
        physics::step_player_with_bodies(world, &mut session.state, input, FIXED_DT, bodies);
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
    mut cursor: Single<&mut CursorOptions>,
) {
    if keys.just_pressed(KeyCode::Escape) {
        cursor.visible = !cursor.visible;
        cursor.grab_mode = if cursor.visible {
            CursorGrabMode::None
        } else {
            CursorGrabMode::Locked
        };
    }
    if !cursor.visible && !options.bot {
        session.yaw = (session.yaw - mouse.delta.x * 0.0025) % std::f32::consts::TAU;
        session.pitch = (session.pitch - mouse.delta.y * 0.0025).clamp(-1.54, 1.54);
    }
    for (key, block) in [
        (KeyCode::Digit1, 1),
        (KeyCode::Digit2, 2),
        (KeyCode::Digit3, 3),
        (KeyCode::Digit4, 4),
        (KeyCode::Digit5, 5),
    ] {
        if keys.just_pressed(key) {
            session.selected = block;
        }
    }
}

fn predict(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    cursor: Single<&CursorOptions>,
    options: Res<Options>,
    world: Res<VoxelWorld>,
    mut session: ResMut<ClientSession>,
    loose: Res<loose_blocks::LooseBlocks>,
) {
    if session.id.is_none() || session.transport.is_none() {
        return;
    }
    if world
        .block(session.state.position.floor().as_ivec3())
        .is_none()
    {
        session.accumulator = 0.0;
        return;
    }
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
        if options.bot {
            // Reproducible traversal for profiling the real rendered client.
            let phase = (session.sequence / 240) % 4;
            movement = match phase {
                0 => [0.0, 1.0],
                1 => [1.0, 0.0],
                2 => [0.0, -1.0],
                _ => [-1.0, 0.0],
            };
            jump = session.sequence.is_multiple_of(90);
        } else if !cursor.visible {
            movement[0] = f32::from(u8::from(keys.pressed(KeyCode::KeyD)))
                - f32::from(u8::from(keys.pressed(KeyCode::KeyA)));
            movement[1] = f32::from(u8::from(keys.pressed(KeyCode::KeyW)))
                - f32::from(u8::from(keys.pressed(KeyCode::KeyS)));
            jump = keys.pressed(KeyCode::Space);
        }
        let input = PlayerInput {
            sequence: session.sequence,
            movement,
            yaw: session.yaw,
            pitch: session.pitch,
            jump,
        };
        let mut state = session.state;
        physics::step_player_with_bodies(&world, &mut state, &input, FIXED_DT, loose.colliders());
        session.state = state;
        session.pending.push_back(input);
    }
    if !session.pending.is_empty() {
        let inputs = session
            .pending
            .iter()
            .skip(session.pending.len().saturating_sub(8))
            .copied()
            .collect();
        let packet = InputPacket {
            session: session.session,
            inputs,
        };
        if let Some(transport) = &mut session.transport
            && let Err(error) = transport.send_inputs(packet)
        {
            session.disconnect(error);
        }
    }
}

fn edit_blocks(
    buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    cursor: Single<&CursorOptions>,
    world: Res<VoxelWorld>,
    mut session: ResMut<ClientSession>,
) {
    if cursor.visible || session.transport.is_none() || session.id.is_none() {
        return;
    }
    let strike = keys.just_pressed(KeyCode::KeyF);
    let block = if strike || buttons.just_pressed(MouseButton::Left) {
        0
    } else if buttons.just_pressed(MouseButton::Right) {
        session.selected
    } else {
        return;
    };
    let origin = session.state.position + Vec3::Y * EYE_HEIGHT;
    let Some(hit) = world.raycast(origin, look_direction(session.yaw, session.pitch), 6.0) else {
        return;
    };
    let target = if block == 0 { hit.block } else { hit.adjacent };
    let Some(chunk) = world.chunks.get(&chunk_coord(target)) else {
        return;
    };
    let expected_revision = chunk.revision;
    session.request += 1;
    let request = session.request;
    if strike {
        session.send(ClientMessage::Strike {
            request,
            target,
            expected_revision,
        });
    } else {
        session.send(ClientMessage::Edit {
            request,
            target,
            block,
            expected_revision,
        });
    }
}

fn present_players(
    time: Res<Time>,
    mut session: ResMut<ClientSession>,
    remotes: Res<RemotePlayers>,
    mut focus: ResMut<RenderFocus>,
    mut camera: Single<&mut Transform, With<PlayerCamera>>,
    mut actors: Query<&mut Transform, Without<PlayerCamera>>,
) {
    session.correction *= (-18.0 * time.delta_secs()).exp();
    camera.translation = session.state.position + Vec3::Y * EYE_HEIGHT + session.correction;
    camera.rotation = Quat::from_rotation_y(session.yaw) * Quat::from_rotation_x(session.pitch);
    focus.0 = session.state.position;
    for remote in remotes.0.values() {
        if let Ok(mut transform) = actors.get_mut(remote.entity) {
            let alpha = (remote.received.elapsed().as_secs_f32() / 0.05).clamp(0.0, 1.0);
            transform.translation =
                remote.previous.lerp(remote.current, alpha) + Vec3::Y * PLAYER_HEIGHT * 0.5;
            transform.rotation = Quat::from_rotation_y(remote.previous_yaw)
                .slerp(Quat::from_rotation_y(remote.yaw), alpha);
        }
    }
}

fn select_voxel(
    world: Res<VoxelWorld>,
    session: Res<ClientSession>,
    mut selection: Single<(&mut Transform, &mut Visibility), With<Selection>>,
) {
    let origin = session.state.position + Vec3::Y * EYE_HEIGHT;
    if let Some(hit) = world.raycast(origin, look_direction(session.yaw, session.pitch), 6.0) {
        selection.0.translation = hit.block.as_vec3() + Vec3::splat(0.5);
        *selection.1 = Visibility::Visible;
    } else {
        *selection.1 = Visibility::Hidden;
    }
}

#[allow(clippy::too_many_arguments)] // Independent Bevy system resources.
fn update_hud(
    mut next_update: Local<Option<Instant>>,
    session: Res<ClientSession>,
    world: Res<VoxelWorld>,
    stats: Res<VoxelRenderStats>,
    remotes: Res<RemotePlayers>,
    loose: Res<loose_blocks::LooseBlocks>,
    diagnostics: Res<DiagnosticsStore>,
    mut hud: Single<&mut Text, With<Hud>>,
) {
    let now = Instant::now();
    if next_update.is_some_and(|next| now < next) {
        return;
    }
    *next_update = Some(now + std::time::Duration::from_millis(200));
    let fps = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|value| value.smoothed())
        .unwrap_or(0.0);
    let material = ["Air", "Grass", "Dirt", "Stone", "Sand", "Wood"][usize::from(session.selected)];
    let action = session
        .last_action
        .map(|result| result.to_string())
        .unwrap_or_default();
    **hud = Text::new(format!(
        "VOXEL / MULTIPLAYER LAB\n{}\n{fps:.0} fps | {} chunks | {} triangles\n{} mesh jobs | {} remote players | {} loose blocks\nxyz {:.1} / {:.1} / {:.1} | tick {}\n{} unacked inputs | actions {} accepted / {} rejected\n{action}\nMATERIAL {} / {material}",
        session.status,
        world.chunks.len(),
        stats.triangles,
        stats.pending_jobs,
        remotes.0.len(),
        loose.count(),
        session.state.position.x,
        session.state.position.y,
        session.state.position.z,
        session.last_tick,
        session.pending.len(),
        session.accepted_edits,
        session.rejected_edits,
        session.selected,
    ));
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
