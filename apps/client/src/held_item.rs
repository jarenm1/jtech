//! First-person held-item viewmodel. The selected hotbar item renders parented
//! to the camera: the package model's node hierarchy once the glTF resolves,
//! else a small cube tinted with the item's block color. Empty hands show
//! nothing. glTF units are meters — models render at authored size.
use bevy::prelude::*;
use protocol::EXPLOSIVE_BOW_ITEM;

use crate::{ClientSession, PlayerCamera, package_assets::PackageAssets};

#[derive(Component)]
struct HeldItem;

#[derive(Default)]
pub(crate) struct HeldState {
    entity: Option<Entity>,
    /// Item the current entity renders; respawn on change.
    item: u32,
    /// The spawned entity renders the package model, not the cube fallback.
    model_loaded: bool,
    cube: Option<Handle<Mesh>>,
}

/// Camera-space rest pose for the held item.
const REST_OFFSET: Vec3 = Vec3::new(0.42, -0.34, -0.62);
/// Swing animation length in seconds.
const SWING_SECONDS: f32 = 0.28;
/// Ticks a ranged weapon draws before firing; matches the server's wind-up.
const DRAW_TICKS: f32 = 60.0;

/// Whether an item draws before firing: a package weapon whose basic attack is
/// ranged. The explosive bow is an admin item that fires on the edge instead.
pub(crate) fn draws(session: &ClientSession, item: u32) -> bool {
    session
        .packages
        .melee_weapons
        .iter()
        .any(|weapon| {
            weapon.item == item && weapon.attack_kind == controller::BasicAttackKind::Ranged
        })
}

/// Keep the held item entity in sync with the selected hotbar slot and the
/// package asset cache, then apply the rest/bob/swing pose every frame.
#[allow(clippy::too_many_arguments)]
pub(crate) fn update(
    mut commands: Commands,
    session: Res<ClientSession>,
    package_assets: Res<PackageAssets>,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut state: Local<HeldState>,
    camera: Query<Entity, With<PlayerCamera>>,
    mut transforms: Query<&mut Transform>,
) {
    let item = session.held_item();

    if item == 0 || session.transport.is_none() {
        if let Some(entity) = state.entity.take() {
            commands.entity(entity).despawn();
        }
        state.item = 0;
        state.model_loaded = false;
        return;
    }

    // Asset resolution (manifest scan, `is_file`, `fs::read`) only runs when
    // the spawned entity is stale: new item, or its model finished resolving
    // after we spawned the cube fallback. A loaded model skips the disk
    // entirely — without this the GLB is reread every rendered frame.
    let model_uri = if state.item != item || !state.model_loaded {
        session
            .packages
            .melee_weapons
            .iter()
            .find(|weapon| weapon.item == item)
            .and_then(|weapon| {
                weapon.model.as_ref().and_then(|path| {
                    package_assets
                        .uri(&weapon.package, path)
                        .and_then(|uri| package_assets.bytes(&weapon.package, path).map(|bytes| (uri, bytes)))
                })
            })
    } else {
        None
    };
    let model_file = model_uri.is_some();

    // Respawn when the item changes or its model finishes resolving.
    if state.item != item || (model_file && !state.model_loaded) {
        if let Some(entity) = state.entity.take() {
            commands.entity(entity).despawn();
        }
    }

    if state.entity.is_none() {
        let Some(camera) = camera.iter().next() else {
            return;
        };
        let spawned = model_uri.as_ref().and_then(|(uri, bytes)| {
            crate::package_assets::spawn_gltf(
                &mut commands,
                &asset_server,
                uri,
                bytes,
                Transform::IDENTITY,
            )
        });
        let child = match spawned {
            Some(entity) => {
                commands.entity(entity).insert(HeldItem);
                entity
            }
            None => {
                let mesh = state
                    .cube
                    .get_or_insert_with(|| meshes.add(Cuboid::new(0.04, 0.04, 0.24)))
                    .clone();
                let [r, g, b, _] = if item == EXPLOSIVE_BOW_ITEM {
                    [0.45, 0.3, 0.15, 1.0]
                } else if item <= u32::from(voxel_world::WOOD) && item >= 1 {
                    voxel_world::block_color(item as u8)
                } else {
                    [0.55, 0.55, 0.6, 1.0]
                };
                let material = materials.add(StandardMaterial {
                    base_color: Color::srgb(r, g, b),
                    perceptual_roughness: 0.7,
                    ..default()
                });
                commands.spawn((Mesh3d(mesh), MeshMaterial3d(material), HeldItem)).id()
            }
        };
        info!(
            "held item spawn: item={item} model={} uri={:?}",
            spawned.is_some(),
            model_uri.as_ref().map(|(uri, _)| uri)
        );
        commands.entity(camera).add_child(child);
        state.entity = Some(child);
        state.item = item;
        state.model_loaded = spawned.is_some();
    }

    // Pose: rest offset plus a gentle bob, and a downward swing arc while
    // `swing_at` is within the animation window.
    let Some(entity) = state.entity else {
        return;
    };
    let Ok(mut transform) = transforms.get_mut(entity) else {
        return;
    };
    let seconds = session.started.elapsed().as_secs_f32();
    let bob = (seconds * 1.7).sin() * 0.012;
    let mut offset = REST_OFFSET + Vec3::Y * bob;
    let mut rotation = Quat::from_rotation_y(-0.25) * Quat::from_rotation_x(0.15);
    if draws(&session, item) {
        // The draw follows the actual charge, so it builds smoothly, holds at
        // full while the arrow is ready, and snaps back when the shot releases.
        let pull = (f32::from(session.state.charge) / DRAW_TICKS).clamp(0.0, 1.0);
        offset += Vec3::new(0.02 * pull, -0.06 * pull, 0.16 * pull);
        rotation *= Quat::from_rotation_x(0.55 * pull) * Quat::from_rotation_z(0.2 * pull);
    }
    if let Some(started) = session.swing_at {
        // A melee swing, or the bow's release kick: a quick forward dip.
        let t = started.elapsed().as_secs_f32() / SWING_SECONDS;
        if t < 1.0 {
            let arc = (t * std::f32::consts::PI).sin();
            offset += Vec3::new(-0.14 * arc, -0.1 * arc, -0.2 * arc);
            rotation *= Quat::from_rotation_x(-1.1 * arc);
        }
    }
    transform.translation = offset;
    transform.rotation = rotation;
}
