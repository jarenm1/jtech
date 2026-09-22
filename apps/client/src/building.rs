//! Presentation of the authoritative building-piece set plus the local
//! placement ghost. Pieces are free-standing boxes, not voxels: `position` is
//! the box center and `yaw_steps` counts quarter turns about Y.
use std::collections::HashMap;

use bevy::{prelude::*, window::CursorOptions};
use gameplay::building::{self, Piece, PieceKind};
use physics::{EYE_HEIGHT, look_direction};
use protocol::PieceSnapshot;
use voxel_world::VoxelWorld;

pub struct BuildingPlugin;
impl Plugin for BuildingPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Building>()
            .add_systems(Startup, setup)
            .add_systems(Update, update_ghost.after(super::edit_blocks));
    }
}

/// Latest replicated piece set. `pieces` mirrors the wire snapshots; `colliders`
/// is the same set as physics boxes for movement prediction.
#[derive(Resource, Default)]
pub struct Building {
    pieces: HashMap<u32, PieceSnapshot>,
    revision: u64,
    colliders: Vec<physics::DynamicCollider>,
    visuals: HashMap<u32, Entity>,
}

/// Per-kind box mesh and material, plus the translucent ghost preview. Meshes
/// are sized to the kind's yaw-0 full extents; yaw rotates the Transform.
#[derive(Resource)]
pub struct BuildingAssets {
    meshes: [Handle<Mesh>; 5],
    materials: [Handle<StandardMaterial>; 5],
    ghost: Handle<StandardMaterial>,
}

/// The single placement-preview entity, rescaled to the candidate piece.
#[derive(Component)]
struct Ghost;

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let color = |kind: PieceKind| match kind {
        PieceKind::Foundation => Color::srgb(0.30, 0.20, 0.12),
        PieceKind::Floor => Color::srgb(0.62, 0.45, 0.27),
        PieceKind::Wall => Color::srgb(0.47, 0.33, 0.20),
        PieceKind::Pillar => Color::srgb(0.42, 0.36, 0.30),
        PieceKind::Bedroll => Color::srgb(0.62, 0.22, 0.18),
    };
    let assets = BuildingAssets {
        meshes: PieceKind::ALL.map(|kind| {
            let extents = kind.half_extents(0) * 2.0;
            meshes.add(Cuboid::new(extents.x, extents.y, extents.z))
        }),
        materials: PieceKind::ALL.map(|kind| {
            materials.add(StandardMaterial {
                base_color: color(kind),
                perceptual_roughness: 1.0,
                ..default()
            })
        }),
        ghost: materials.add(StandardMaterial {
            base_color: Color::srgba(0.4, 0.9, 0.4, 0.35),
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            ..default()
        }),
    };
    // Unit cuboid scaled to the candidate's full extents each frame.
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::from_length(1.0))),
        MeshMaterial3d(assets.ghost.clone()),
        Visibility::Hidden,
        Ghost,
    ));
    commands.insert_resource(assets);
}

impl Building {
    pub fn colliders(&self) -> &[physics::DynamicCollider] {
        &self.colliders
    }
    pub fn pieces(&self) -> &HashMap<u32, PieceSnapshot> {
        &self.pieces
    }
    /// The replicated set as gameplay pieces for raycasts and clearance checks.
    pub(crate) fn gameplay_pieces(&self) -> Vec<Piece> {
        self.pieces().values().map(piece).collect()
    }

    /// Replace the set with a newer revision, spawning/despawning visuals and
    /// rebuilding colliders. Stale or duplicate revisions are ignored.
    pub fn receive(
        &mut self,
        revision: u64,
        pieces: &[PieceSnapshot],
        commands: &mut Commands,
        assets: &BuildingAssets,
    ) {
        if self.revision != 0 && revision <= self.revision {
            return;
        }
        self.revision = revision;
        self.pieces = pieces.iter().map(|piece| (piece.id, *piece)).collect();
        self.colliders = self
            .pieces
            .values()
            .filter(|piece| piece.position.is_finite())
            .map(|piece| physics::DynamicCollider {
                id: piece.id,
                position: piece.position,
                velocity: Vec3::ZERO,
                half_extents: piece.kind.half_extents(piece.yaw_steps),
            })
            .collect();
        self.colliders.sort_unstable_by_key(|collider| collider.id);
        self.visuals.retain(|id, entity| {
            if self.pieces.contains_key(id) {
                true
            } else {
                commands.entity(*entity).despawn();
                false
            }
        });
        for piece in self.pieces.values() {
            if !piece.position.is_finite() {
                continue;
            }
            let transform = Transform::from_translation(piece.position)
                .with_rotation(Quat::from_rotation_y(
                    f32::from(piece.yaw_steps) * std::f32::consts::FRAC_PI_2,
                ));
            if let Some(entity) = self.visuals.get(&piece.id) {
                commands.entity(*entity).insert(transform);
            } else {
                let entity = commands
                    .spawn((
                        Mesh3d(assets.meshes[piece.kind as usize].clone()),
                        MeshMaterial3d(assets.materials[piece.kind as usize].clone()),
                        transform,
                    ))
                    .id();
                self.visuals.insert(piece.id, entity);
            }
        }
    }

    /// Drop every piece and visual on session (re)join.
    pub fn clear(&mut self, commands: &mut Commands) {
        for entity in self.visuals.drain().map(|(_, entity)| entity) {
            commands.entity(entity).despawn();
        }
        self.pieces.clear();
        self.colliders.clear();
        self.revision = 0;
    }
}

/// Snapshot to gameplay piece; `position` stays the box center.
fn piece(snapshot: &PieceSnapshot) -> Piece {
    Piece {
        id: snapshot.id,
        kind: snapshot.kind,
        position: snapshot.position,
        yaw_steps: snapshot.yaw_steps,
        health: snapshot.health,
    }
}

/// Preview the piece the selected slot would place: pose against the aimed
/// surface, tinted by whether the box would fit. Hidden when no structural
/// slot is held or nothing is in reach.
#[allow(clippy::too_many_arguments)] // Preview reads input, world and piece state.
fn update_ghost(
    world: Res<VoxelWorld>,
    session: Res<crate::ClientSession>,
    building: Res<Building>,
    cursor: Single<&CursorOptions>,
    menu: Res<crate::pause_menu::PauseMenu>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    assets: Res<BuildingAssets>,
    mut ghost: Single<(&mut Transform, &mut Visibility), With<Ghost>>,
) {
    let hide = |ghost: &mut Single<(&mut Transform, &mut Visibility), With<Ghost>>| {
        *ghost.1 = Visibility::Hidden;
    };
    let Some(kind) = building::slot_piece(session.selected) else {
        return hide(&mut ghost);
    };
    if menu.blocks_gameplay() || cursor.visible || session.health.is_depleted() {
        return hide(&mut ghost);
    }
    let origin = session.state.position + Vec3::Y * EYE_HEIGHT;
    let direction = look_direction(session.yaw, session.pitch);
    let Some(hit) = world.raycast(origin, direction, building::BUILD_REACH) else {
        return hide(&mut ghost);
    };
    let Some(pose) = building::placement_pose(
        kind,
        session.yaw_steps,
        origin + direction * hit.distance,
        building::hit_normal(&hit),
    ) else {
        return hide(&mut ghost);
    };
    let pieces = building.gameplay_pieces();
    let clear = building::placement_clear(
        &world,
        kind,
        session.yaw_steps,
        pose,
        pieces.iter(),
        std::iter::once(&session.state),
    );
    let extents = kind.half_extents(session.yaw_steps) * 2.0;
    ghost.0.translation = pose;
    ghost.0.rotation =
        Quat::from_rotation_y(f32::from(session.yaw_steps) * std::f32::consts::FRAC_PI_2);
    ghost.0.scale = extents;
    *ghost.1 = Visibility::Visible;
    if let Some(material) = materials.get_mut(&assets.ghost) {
        material.base_color = if clear.is_ok() {
            Color::srgba(0.4, 0.9, 0.4, 0.35)
        } else {
            Color::srgba(0.9, 0.3, 0.25, 0.35)
        };
    }
}

/// Default-handle assets for tests that exercise `receive` without rendering.
#[cfg(test)]
pub(crate) fn test_assets() -> BuildingAssets {
    BuildingAssets {
        meshes: std::array::from_fn(|_| Handle::default()),
        materials: std::array::from_fn(|_| Handle::default()),
        ghost: Handle::default(),
    }
}
