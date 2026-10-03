use controller::PlayerInput;
mod bow_power_hud;
mod death_overlay;
mod drops;
mod game_hud;
mod health_hud;
mod held_item;
mod inventory_ui;
mod lighting;
mod loose_blocks;
mod package_hud;
mod package_assets;
mod pause_menu;
mod projectiles;
mod scatter;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    time::{Duration, Instant},
};

use bevy::{
    app::{AppExit, ScheduleRunnerPlugin},
    camera::RenderTarget,
    image::BevyDefault,
    input::mouse::AccumulatedMouseMotion,
    prelude::*,
    render::{
        RenderPlugin,
        render_resource::{TextureFormat, TextureUsages},
        settings::{RenderCreation, WgpuSettings},
        view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk},
    },
    window::{CursorGrabMode, CursorOptions, ExitCondition, PresentMode, WindowRef},
    winit::WinitPlugin,
};
use image::codecs::gif::{GifEncoder, Repeat};
use networking::ClientTransport;
use physics::{EYE_HEIGHT, FIXED_DT, PLAYER_HEIGHT, PlayerState, look_direction};
use protocol::{
    BowPower, ClientMessage, EXPLOSIVE_BOW_ITEM, EditRejection, Health, InputBatch, InputPacket,
    Inventory, MAX_INPUT_BATCH, ServerMessage, Snapshot,
};
use voxel_render::{RenderFocus, VoxelRenderPlugin, VoxelRenderStats};
use voxel_world::{
    CHUNK_SIZE, Chunk, VoxelWorld, WORLD_MAX_Y, WORLD_MIN_Y, WorldPlugin, chunk_coord,
};

/// Which wgpu adapter the renderer should use. `Software` forces the fallback
/// (lavapipe/llvmpipe) adapter so captures run on CPU without touching the GPU.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum RenderBackend {
    #[default]
    Auto,
    Software,
    Gpu,
}

#[derive(Resource)]
struct Options {
    server: SocketAddr,
    bot: bool,
    frames: Option<u64>,
    screenshot: Option<String>,
    lighting: lighting::DayCycle,
    /// Render offscreen to an image instead of a window. Implies `bot`.
    headless: bool,
    width: u32,
    height: u32,
    backend: RenderBackend,
    /// Animated GIF of the run, sampled at `clip_fps`.
    clip: Option<String>,
    clip_fps: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            server: "127.0.0.1:4000".parse().unwrap(),
            bot: false,
            frames: None,
            screenshot: None,
            lighting: lighting::DayCycle::default(),
            headless: false,
            width: 1280,
            height: 800,
            backend: RenderBackend::Auto,
            clip: None,
            clip_fps: 20,
        }
    }
}

impl Options {
    fn parse() -> Result<Self, String> {
        let mut options = Self::default();
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
                "--headless" => options.headless = true,
                "--width" => {
                    options.width = args
                        .next()
                        .ok_or("--width needs a pixel count")?
                        .parse()
                        .map_err(|_| "invalid width")?;
                    if options.width == 0 {
                        return Err("--width must be positive".into());
                    }
                }
                "--height" => {
                    options.height = args
                        .next()
                        .ok_or("--height needs a pixel count")?
                        .parse()
                        .map_err(|_| "invalid height")?;
                    if options.height == 0 {
                        return Err("--height must be positive".into());
                    }
                }
                "--render-backend" => {
                    options.backend = match args.next().as_deref() {
                        Some("auto") => RenderBackend::Auto,
                        Some("software") => RenderBackend::Software,
                        Some("gpu") => RenderBackend::Gpu,
                        _ => return Err("--render-backend expects auto|software|gpu".into()),
                    };
                }
                "--clip" => options.clip = Some(args.next().ok_or("--clip needs a .gif path")?),
                "--clip-fps" => {
                    let fps = args
                        .next()
                        .ok_or("--clip-fps needs a frame rate")?
                        .parse::<u32>()
                        .map_err(|_| "invalid frame rate")?;
                    if !(1..=60).contains(&fps) {
                        return Err("--clip-fps must be 1..=60".into());
                    }
                    options.clip_fps = fps;
                }
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
                        "voxel-client [--server IP:PORT] [--bot] [--frames N] [--screenshot PATH.png] [--clip PATH.gif] [--clip-fps N] [--time-of-day HOUR] [--day-length SECONDS]\nHeadless capture: [--headless] [--width N] [--height N] [--render-backend auto|software|gpu]\n  --headless renders offscreen to an image (implies --bot) and needs no display server.\n  --render-backend software forces the CPU fallback adapter (lavapipe/llvmpipe): no GPU use.\n  --clip writes an animated GIF sampled at --clip-fps (default 20); it plays inline in a PR body.\nLighting: starts at 09:00, 1200 seconds/day; --day-length 0 freezes time.\nWASD move | mouse look | Space jump/up | V noclip flight | Ctrl descend | left/right click hit (bow: hold right to shoot, R cycle power) | F debug launch (GPU server) | 1-0 hotbar | Tab inventory | Esc pause menu | F12 screenshot"
                    );
                    std::process::exit(0);
                }
                _ => return Err(format!("unknown argument: {arg}")),
            }
        }
        if options.headless {
            // No window means no keyboard or mouse, so the scripted traversal is
            // the only input source: headless always drives the bot.
            options.bot = true;
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
    /// When the last melee swing started, for the held-item swing animation.
    swing_at: Option<Instant>,
    pending: VecDeque<PlayerInput>,
    sequence: u64,
    last_tick: u64,
    accumulator: f32,
    yaw: f32,
    pitch: f32,
    correction: Vec3,
    /// Selected hotbar slot, 1-10.
    selected: u8,
    /// Item id assigned to each hotbar slot; `None` is an empty slot.
    hotbar: [Option<u32>; 10],
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
            swing_at: None,
            sequence: 0,
            last_tick: 0,
            accumulator: 0.0,
            yaw: 0.0,
            pitch: -0.25,
            correction: Vec3::ZERO,
            selected: 6,
            hotbar: {
                let mut hotbar = [None; 10];
                hotbar[5] = Some(EXPLOSIVE_BOW_ITEM);
                hotbar
            },
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
    /// Item id the selected hotbar slot holds; 0 means empty hands.
    fn held_item(&self) -> u32 {
        self.hotbar
            .get(usize::from(self.selected) - 1)
            .copied()
            .flatten()
            .unwrap_or(0)
    }

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
    fn input_packet(&self, inputs: InputBatch) -> InputPacket {
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
        }].into()))
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

// Scratch id sets rebuilt per snapshot so retains stay O(existing + snapshot)
// instead of scanning the snapshot for every tracked remote.
#[derive(Default)]
struct SnapshotIds {
    players: HashSet<u64>,
    actors: HashSet<u32>,
}

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

/// Window- and cursor-coupled systems only run when a real window exists:
/// `--headless` spawns no window entity and has no input devices.
fn windowed(options: Res<Options>) -> bool {
    !options.headless
}

struct ClientPlugin;

impl Plugin for ClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(controller::ControllerPlugin::default());
        app.add_systems(
            FixedUpdate,
            observe_character_bodies.in_set(controller::ControllerSet::Intent),
        );
        app.init_resource::<ClientSession>()
            .init_resource::<RemotePlayers>()
            .init_resource::<RemoteActors>()
            .init_resource::<pause_menu::PauseMenu>()
            .init_resource::<death_overlay::DeathOverlay>()
            .init_resource::<inventory_ui::InventoryUi>()
            .init_resource::<package_assets::PackageAssets>()
            .init_resource::<scatter::ScatterWorld>()
            .init_resource::<ClipFrames>()
            .add_systems(Startup, setup)
            .add_systems(
                Update,
                (
                    inventory_ui::input.run_if(windowed),
                    inventory_ui::sync,
                    inventory_ui::drag.run_if(windowed),
                )
                    .chain()
                    .before(pause_menu::sync),
            )
            .add_systems(Update, (capture_frames, record_metrics))
            .add_systems(
                Update,
                (
                    receive_network,
                    pause_menu::input.run_if(windowed),
                    pause_menu::actions,
                    pause_menu::sync,
                    death_overlay::input,
                    death_overlay::actions,
                    death_overlay::sync,
                    pause_menu::sync_cursor.run_if(windowed),
                    controls.run_if(windowed),
                    bow_power_hud::cycle_on_click.run_if(pause_menu::gameplay_enabled),
                    predict,
                    edit_blocks,
                    held_item::update,
                    present_players,
                    game_hud::update,
                    game_hud::update_fps,
                    health_hud::update,
                    bow_power_hud::update,
                    package_hud::update,
                    scatter::sync_scatter,
                )
                    .chain(),
            )
            .add_systems(
                Update,
                auto_hotbar
                    .after(receive_network)
                    .before(controls),
            );
    }
}

fn main() {
    let options = Options::parse().unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(2)
    });
    let headless = options.headless;
    let backend = options.backend;
    let cache = package_assets::PackageAssets::cache_dir();
    let _ = std::fs::create_dir_all(&cache);
    let mut app = App::new();
    // Asset sources must register before AssetPlugin (inside DefaultPlugins);
    // `pkg://` serves downloaded package files from the cache dir.
    app.register_asset_source(
        bevy::asset::io::AssetSourceId::from("pkg"),
        bevy::asset::io::AssetSourceBuilder::platform_default(
            cache.to_str().unwrap_or("/tmp/voxel-package-assets"),
            None,
        ),
    );
    let mut plugins = DefaultPlugins.set(WindowPlugin {
        primary_window: if headless {
            None
        } else {
            Some(Window {
                title: "Voxel".into(),
                resolution: (1280, 800).into(),
                present_mode: PresentMode::AutoVsync,
                ..default()
            })
        },
        // Headless has no window to close, so exit is driven by `--frames`.
        exit_condition: if headless {
            ExitCondition::DontExit
        } else {
            ExitCondition::OnAllClosed
        },
        ..default()
    });
    if headless {
        // WinitPlugin panics without a display server; ScheduleRunnerPlugin
        // drives the loop instead of the winit event loop.
        plugins = plugins.disable::<WinitPlugin>();
    }
    if backend != RenderBackend::Auto {
        plugins = plugins.set(RenderPlugin {
            render_creation: RenderCreation::Automatic(WgpuSettings {
                force_fallback_adapter: backend == RenderBackend::Software,
                ..default()
            }),
            ..default()
        });
    }
    app.insert_resource(options.lighting)
        .insert_resource(options)
        .add_plugins(plugins)
        .add_plugins((
            bevy::diagnostic::FrameTimeDiagnosticsPlugin::default(),
            WorldPlugin,
            VoxelRenderPlugin,
            lighting::LightingPlugin,
            ClientPlugin,
            loose_blocks::LooseBlocksPlugin,
            projectiles::ProjectilesPlugin,
            drops::DropsPlugin,
        ));
    if headless {
        app.add_plugins(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    }
    app.run();
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

/// Offscreen render target for `--headless`. Kept as a resource so
/// `capture_screenshot` can read the same image back.
#[derive(Resource)]
struct HeadlessTarget(Handle<Image>);

/// A render-attachment image the camera draws into, with `COPY_SRC` so the
/// screenshot path can read it back to the CPU.
fn headless_target(width: u32, height: u32) -> Image {
    let mut image = Image::new_target_texture(width, height, TextureFormat::bevy_default());
    image.texture_descriptor.usage |= TextureUsages::COPY_SRC;
    image
}

fn setup(
    mut commands: Commands,
    options: Res<Options>,
    mut session: ResMut<ClientSession>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    cursor: Option<Single<&mut CursorOptions>>,
) {
    match ClientTransport::connect(options.server) {
        Ok(transport) => {
            session.transport = Some(transport);
            session.status = format!("Joining {}", options.server);
        }
        Err(error) => session.disconnect(error),
    }
    // Headless has no window entity, so `CursorOptions` does not exist.
    if let Some(mut cursor) = cursor {
        cursor.grab_mode = if options.bot {
            CursorGrabMode::None
        } else {
            CursorGrabMode::Locked
        };
        cursor.visible = options.bot;
    }
    let target = if options.headless {
        let handle = images.add(headless_target(options.width, options.height));
        commands.insert_resource(HeadlessTarget(handle.clone()));
        RenderTarget::from(handle)
    } else {
        RenderTarget::Window(WindowRef::Primary)
    };
    let mut camera = commands.spawn((
        Camera3d::default(),
        Camera {
            target,
            ..default()
        },
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
    if options.headless {
        // `DefaultUiCamera` only accepts window targets, so an offscreen camera
        // is never picked implicitly and the HUD would not render at all.
        camera.insert(IsDefaultUiCamera);
    }
    commands.insert_resource(ActorAssets {
        mesh: meshes.add(Cuboid::new(0.6, PLAYER_HEIGHT, 0.6)),
        material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.2, 0.55, 0.95),
            perceptual_roughness: 1.0,
            ..default()
        }),
        dummy_mesh: meshes.add(Capsule3d::new(0.32, 1.1)),
    });
    inventory_ui::spawn(&mut commands);
    game_hud::spawn(&mut commands);
    pause_menu::spawn(&mut commands);
    death_overlay::spawn(&mut commands, &mut images);
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
    mut package_assets: ResMut<package_assets::PackageAssets>,
    mut scatter: ResMut<scatter::ScatterWorld>,
    mut snapshot_ids: Local<SnapshotIds>,
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
                scatter_species,
            } => {
                projectiles.clear(&mut commands);
                drops.clear(&mut commands);
                scatter.begin_session(&mut commands, scatter_species);
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
                material_runs,
                density_runs,
                scatter: instances,
            } => match Chunk::from_voxel_runs(revision, &material_runs, &density_runs) {
                Ok(chunk) => {
                    world.insert(coord, chunk);
                    scatter.receive(coord, instances);
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
                voxels,
            } => {
                let valid = world
                    .chunks
                    .get(&coord)
                    .is_some_and(|chunk| chunk.revision == from)
                    && to == from.saturating_add(voxels.len() as u64)
                    && voxels.iter().all(|(cell, voxel)| {
                        usize::from(*cell) < voxel_world::CHUNK_VOLUME
                            && voxel.material <= voxel_world::WOOD
                    });
                if !valid {
                    session.resync(coord);
                    continue;
                }
                for (cell, voxel) in voxels {
                    let cell = i32::from(cell);
                    let local = IVec3::new(
                        cell % CHUNK_SIZE,
                        cell / (CHUNK_SIZE * CHUNK_SIZE),
                        (cell / CHUNK_SIZE) % CHUNK_SIZE,
                    );
                    let _ = world.set_voxel(
                        coord * CHUNK_SIZE + local,
                        voxel_world::Voxel {
                            material: voxel.material,
                            density: voxel.density,
                            placed: voxel.placed,
                        },
                    );
                }
                if world.chunks[&coord].revision != to {
                    session.resync(coord);
                }
            }
            ServerMessage::Forget { coord } => {
                world.remove(coord);
                scatter.forget(coord, &mut commands);
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
                melee_weapons,
                assets,
            } => {
                session
                    .packages
                    .receive(revision, packages, bow_shots_per_second, melee_weapons);
                for request in package_assets.sync(assets) {
                    session.send(request);
                }
            }
            ServerMessage::AssetData {
                package,
                path,
                offset,
                total,
                data,
            } => {
                package_assets.receive_chunk(&package, &path, offset, total, &data);
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
                    &session.packages,
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
        snapshot_ids.players.clear();
        snapshot_ids
            .players
            .extend(snapshot.players.iter().map(|player| player.id));
        snapshot_ids.actors.clear();
        snapshot_ids
            .actors
            .extend(snapshot.actors.iter().map(|actor| actor.id));
        remotes.0.retain(|id, remote| {
            if snapshot_ids.players.contains(id) {
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
            if snapshot_ids.actors.contains(id) {
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
    if !options.bot && !cursor.visible && keys.just_pressed(KeyCode::KeyR) {
        if session.held_item() == EXPLOSIVE_BOW_ITEM {
            session.bow_power = session.bow_power.next();
        }
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
    cursor: Option<Single<&CursorOptions>>,
    options: Res<Options>,
    world: Res<VoxelWorld>,
    mut session: ResMut<ClientSession>,
    loose: Res<loose_blocks::LooseBlocks>,
    menu: Res<pause_menu::PauseMenu>,
) {
    // Headless spawns no window entity, so there is no `CursorOptions`: treat
    // the cursor as hidden, matching what the bot path already assumes.
    let cursor_visible = cursor.is_some_and(|cursor| cursor.visible);
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
        && !cursor_visible
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
        !options.bot && !cursor_visible && !menu.blocks_gameplay(),
        keys.just_pressed(KeyCode::Space),
    );
    session.accumulator += time.delta_secs().min(0.1);
    let colliders = loose.colliders();
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
        } else if !cursor_visible && !menu.blocks_gameplay() {
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
            selected: session.held_item(),
            jump,
            descend,
            noclip: session.noclip_requested,
            attack: session.consume_attack(),
        };
        session.predict_input(&world, input, colliders);
    }
    if !session.pending.is_empty() {
        let mut inputs = InputBatch::new();
        for input in session
            .pending
            .iter()
            .skip(session.pending.len().saturating_sub(MAX_INPUT_BATCH))
        {
            inputs.push(*input);
        }
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
    cursor: Option<Single<&CursorOptions>>,
    world: Res<VoxelWorld>,
    mut session: ResMut<ClientSession>,
    actors: Res<RemoteActors>,
    remotes: Res<RemotePlayers>,
    time: Res<Time>,
    mut bow_repeat: Local<BowRepeat>,
    menu: Res<pause_menu::PauseMenu>,
) {
    // Headless spawns no window entity, so there is no `CursorOptions`: treat
    // the cursor as hidden, matching what the bot path already assumes.
    let cursor_visible = cursor.is_some_and(|cursor| cursor.visible);
    if menu.blocks_gameplay()
        || cursor_visible
        || session.transport.is_none()
        || session.id.is_none()
        || session.health.is_depleted()
    {
        *bow_repeat = BowRepeat::default();
        return;
    }
    let strike = keys.just_pressed(KeyCode::KeyF);
    // Melee: a living actor or remote player under the crosshair within reach
    // sets the attack flag instead of mining. The server re-validates; this
    // only routes input. The swing animation plays on every left click —
    // mining, whiffing, or hitting — so the held item always reacts.
    let origin = session.state.position + Vec3::Y * EYE_HEIGHT;
    let direction = look_direction(session.yaw, session.pitch);
    let melee = buttons.pressed(MouseButton::Left)
        && melee_target(&world, origin, direction, &actors, &remotes, &session);
    if melee {
        session.attack_pending = true;
    }
    if buttons.just_pressed(MouseButton::Left) {
        session.swing_at = Some(Instant::now());
    }
    let hit = buttons.just_pressed(MouseButton::Left) && !melee;
    let bow_shot = repeat_bow(
        &mut bow_repeat,
        time.elapsed_secs_f64(),
        session.held_item() == EXPLOSIVE_BOW_ITEM
            && buttons.pressed(MouseButton::Right)
            && !strike
            && !hit,
        session.packages.bow_shots_per_second,
    );
    let secondary = if session.held_item() == EXPLOSIVE_BOW_ITEM {
        bow_shot
    } else {
        buttons.just_pressed(MouseButton::Right)
    };
    if let Some(message) = block_action(&mut session, &world, strike, hit, secondary) {
        session.send(message);
    }
}

/// True when a living actor or remote player is under the crosshair within
fn melee_target(
    world: &VoxelWorld,
    origin: Vec3,
    direction: Vec3,
    actors: &RemoteActors,
    remotes: &RemotePlayers,
    session: &ClientSession,
) -> bool {
    let range = session.packages.melee_range(session.held_item());
    let wall = world
        .raycast(origin, direction, range)
        .map(|hit| hit.distance)
        .unwrap_or(range);
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
    if !strike && !hit && secondary && session.held_item() == EXPLOSIVE_BOW_ITEM {
        session.request += 1;
        return Some(ClientMessage::FireBow {
            request: session.request,
            yaw: session.yaw,
            pitch: session.pitch,
            power: session.bow_power,
        });
    }
    if !strike && !hit {
        return None;
    }
    let origin = session.state.position + Vec3::Y * EYE_HEIGHT;
    let direction = look_direction(session.yaw, session.pitch);
    let hit = world.raycast(origin, direction, 6.0)?;
    let expected_revision = world.chunks.get(&chunk_coord(hit.block))?.revision;
    session.request += 1;
    let request = session.request;
    Some(if strike {
        ClientMessage::Strike {
            request,
            target: hit.block,
            expected_revision,
        }
    } else {
        ClientMessage::Edit {
            request,
            target: hit.block,
            block: 0,
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


/// Frames to skip before the first clip sample: the scene has not rendered yet,
/// so early frames come out black.
const CLIP_WARMUP_FRAMES: u64 = 30;

/// Frames sampled for `--clip`, encoded to an animated GIF when the run ends.
#[derive(Resource, Default)]
struct ClipFrames {
    frames: Vec<image::RgbaImage>,
    /// Next render frame to sample, so the clip plays at `--clip-fps` no matter
    /// how fast the renderer actually runs.
    next: u64,
}

/// The camera's render target: offscreen when `--headless`, else the window.
fn screenshot_target(target: &Option<Res<HeadlessTarget>>) -> Screenshot {
    match target {
        Some(target) => Screenshot::image(target.0.clone()),
        None => Screenshot::primary_window(),
    }
}

/// One capture per render target per frame: the screenshot plugin drops
/// duplicates, so the one-shot still wins over a clip sample.
fn capture_frames(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    options: Res<Options>,
    target: Option<Res<HeadlessTarget>>,
    mut session: ResMut<ClientSession>,
    mut clip: ResMut<ClipFrames>,
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
            .spawn(screenshot_target(&target))
            .observe(save_to_disk(path.clone()));
        session.captured = true;
        info!("CAPTURE {path}");
        return;
    }
    if options.clip.is_some() && session.frame >= clip.next {
        // Skip the first frames (nothing has rendered yet, they come out black)
        // and the last few (the capture would land after the app exits).
        let warmup = CLIP_WARMUP_FRAMES;
        let end = options.frames.map(|frames| frames.saturating_sub(3));
        if session.frame >= warmup && end.is_none_or(|end| session.frame < end) {
            let stride = u64::from(60 / options.clip_fps.max(1)).max(1);
            clip.next = session.frame + stride;
            commands
                .spawn(screenshot_target(&target))
                .observe(collect_clip_frame);
        }
    }
}

/// Observer: append each sampled frame to the clip buffer.
fn collect_clip_frame(captured: On<ScreenshotCaptured>, mut clip: ResMut<ClipFrames>) {
    match captured.image.clone().try_into_dynamic() {
        Ok(frame) => clip.frames.push(frame.to_rgba8()),
        Err(error) => warn!("clip frame dropped: {error}"),
    }
}

/// Encode the sampled frames as a looping GIF. GIF plays inline in a PR body,
/// unlike a video file, which needs a browser upload.
fn write_clip(path: &str, fps: u32, frames: &[image::RgbaImage]) {
    let file = match std::fs::File::create(path) {
        Ok(file) => file,
        Err(error) => {
            error!("clip: cannot create {path}: {error}");
            return;
        }
    };
    let mut encoder = GifEncoder::new(file);
    if let Err(error) = encoder.set_repeat(Repeat::Infinite) {
        error!("clip: cannot set repeat: {error}");
        return;
    }
    for frame in frames {
        let delay = image::Delay::from_numer_denom_ms(1000, fps);
        let frame = image::Frame::from_parts(frame.clone(), 0, 0, delay);
        if let Err(error) = encoder.encode_frame(frame) {
            error!("clip: encode failed: {error}");
            return;
        }
    }
    info!("CLIP {path} frames={}", frames.len());
}
/// Granted equipment lands in empty hotbar slots so spawn loadouts are usable
fn auto_hotbar(mut session: ResMut<ClientSession>, options: Res<Options>) {
    let weapons: Vec<u32> = session
        .packages
        .melee_weapons
        .iter()
        .map(|weapon| weapon.item)
        .collect();
    if weapons.is_empty() {
        return;
    }
    for item in weapons {
        if session.inventory.count(item) == 0
            || session.hotbar.iter().flatten().any(|held| *held == item)
        {
            continue;
        }
        if let Some(slot) = session.hotbar.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(item);
        }
    }
    if options.bot {
        // Bots can't pick slots; hold a weapon that ships a model so rendered
        // captures exercise the package asset path.
        if let Some(slot) = session
            .packages
            .melee_weapons
            .iter()
            .find(|weapon| weapon.model.is_some())
            .and_then(|weapon| {
                session.hotbar.iter().position(|held| *held == Some(weapon.item))
            })
        {
            session.selected = slot as u8 + 1;
        }
    }
}

fn record_metrics(
    time: Res<Time>,
    options: Res<Options>,
    world: Res<VoxelWorld>,
    stats: Res<VoxelRenderStats>,
    scatter: Res<scatter::ScatterWorld>,
    clip: Res<ClipFrames>,
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
            "CLIENT_METRICS elapsed_s={:.1} frames={} p50_ms={:.2} p95_ms={:.2} p99_ms={:.2} chunks={} meshes={} triangles={} jobs={} upload_bytes={} stale_jobs={} pending_inputs={} scatter={} grass={} max_correction={:.3}",
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
            scatter.instance_count(),
            stats.grass_blades,
            session.max_correction
        );
        session.frame_times.clear();
        session.next_metrics = Instant::now() + std::time::Duration::from_secs(10);
    }
    if ending {
        if let Some(path) = &options.clip {
            write_clip(path, options.clip_fps, &clip.frames);
        }
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
