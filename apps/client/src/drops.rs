//! Presentation of authoritative dropped items: interpolated cubes that bob and
//! spin on the client. Clients hold no drop or inventory state of their own.
use std::{collections::HashSet, time::Instant};

use bevy::prelude::*;
use protocol::{DropSnapshot, MAX_DROPS};

pub struct DropsPlugin;
impl Plugin for DropsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Drops>()
            .add_systems(Startup, setup)
            .add_systems(Update, present.after(super::receive_network))
            .add_systems(Update, upgrade_models.after(super::receive_network));
    }
}

/// Half-height of a dropped item; matches the server rest offset.
const DROP_RADIUS: f32 = 0.15;
const SNAPSHOT_SECONDS: f32 = 0.05;

struct DropVisual {
    entity: Entity,
    previous: Vec3,
    current: Vec3,
    received: Instant,
    phase: f32,
    /// Authored model the drop wants once its file is downloaded.
    model: Option<(String, String)>,
    /// The spawned entity is already the model scene.
    model_loaded: bool,
}


#[derive(Resource, Default)]
pub struct Drops {
    items: std::collections::HashMap<u32, DropVisual>,
    tick: Option<u64>,
    /// Scratch: indices into the latest snapshot slice that passed validation.
    scratch_valid: Vec<usize>,
    /// Scratch: ids in the latest snapshot, for despawn reconciliation.
    scratch_ids: HashSet<u32>,
}

#[derive(Resource)]
pub struct DropAssets {
    mesh: Handle<Mesh>,
    materials: Vec<Handle<StandardMaterial>>,
    /// Neutral swatch for equipment items without an authored model.
    equipment: Handle<StandardMaterial>,
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.insert_resource(DropAssets {
        mesh: meshes.add(Cuboid::from_length(DROP_RADIUS * 2.0)),
        materials: (0..=voxel_world::WOOD)
            .map(|id| {
                let [r, g, b, _] = voxel_world::block_color(id);
                materials.add(StandardMaterial {
                    base_color: Color::srgb(r, g, b),
                    perceptual_roughness: 1.0,
                    ..default()
                })
            })
            .collect(),
        equipment: materials.add(StandardMaterial {
            base_color: Color::srgb(0.55, 0.55, 0.6),
            perceptual_roughness: 0.6,
            ..default()
        }),
    });
}

impl Drops {
    pub fn clear(&mut self, commands: &mut Commands) {
        for (_, item) in self.items.drain() {
            commands.entity(item.entity).despawn();
        }
        self.tick = None;
    }

    pub fn receive(
        &mut self,
        tick: u64,
        drops: &[DropSnapshot],
        commands: &mut Commands,
        assets: &DropAssets,
        packages: &crate::package_hud::ServerPackages,
        now: Instant,
    ) {
        if self.tick.is_some_and(|old| tick <= old) {
            return;
        }
        self.tick = Some(tick);
        self.scratch_valid.clear();
        self.scratch_valid.extend(
            drops
                .iter()
                .take(MAX_DROPS)
                .enumerate()
                .filter(|(_, drop)| {
                    drop.position.is_finite()
                        && drop.count > 0
                        && ((1..=u32::from(voxel_world::WOOD)).contains(&drop.item)
                            || packages.is_equipment(drop.item))
                })
                .map(|(index, _)| index),
        );
        self.scratch_ids.clear();
        self.scratch_ids
            .extend(self.scratch_valid.iter().map(|&index| drops[index].id));
        self.items.retain(|id, visual| {
            if self.scratch_ids.contains(id) {
                true
            } else {
                commands.entity(visual.entity).despawn();
                false
            }
        });
        for &index in &self.scratch_valid {
            let drop = &drops[index];
            self.items
                .entry(drop.id)
                .and_modify(|visual| {
                    // Continue from the displayed position if snapshots arrive in a burst.
                    let alpha = (visual.received.elapsed().as_secs_f32() / SNAPSHOT_SECONDS)
                        .clamp(0.0, 1.0);
                    visual.previous = visual.previous.lerp(visual.current, alpha);
                    visual.current = drop.position;
                    visual.received = now;
                })
                .or_insert_with(|| {
                    // Resolve the authored model only for newly spawned drops;
                    // existing visuals keep the model they were created with.
                    let model = packages
                        .melee_weapons
                        .iter()
                        .find(|weapon| weapon.item == drop.item)
                        .and_then(|weapon| {
                            weapon
                                .model
                                .as_ref()
                                .map(|path| (weapon.package.clone(), path.clone()))
                        });
                    // Cube placeholder; `upgrade_models` swaps in the authored
                    // model once the glTF resolves.
                    let material = assets
                        .materials
                        .get(drop.item as usize)
                        .unwrap_or(&assets.equipment);
                    DropVisual {
                        entity: commands
                            .spawn((
                                Mesh3d(assets.mesh.clone()),
                                MeshMaterial3d(material.clone()),
                                Transform::from_translation(drop.position),
                            ))
                            .id(),
                        previous: drop.position,
                        current: drop.position,
                        received: now,
                        phase: drop.id as f32 * 0.7,
                        model,
                        model_loaded: false,
                    }
                });
        }
    }
}

/// Swap a placeholder cube for the authored model once its file is in the
/// package asset cache.
fn upgrade_models(
    mut commands: Commands,
    mut drops: ResMut<Drops>,
    package_assets: Res<crate::package_assets::PackageAssets>,
    asset_server: Res<AssetServer>,
) {
    for visual in drops.items.values_mut() {
        if visual.model_loaded {
            continue;
        }
        let Some((package, path)) = &visual.model else {
            continue;
        };
        let (Some(uri), Some(bytes)) = (
            package_assets.uri(package, path),
            package_assets.bytes(package, path),
        ) else {
            continue;
        };
        let Some(entity) = crate::package_assets::spawn_gltf(
            &mut commands,
            &asset_server,
            &uri,
            &bytes,
            Transform::from_translation(visual.current),
            None,
        ) else {
            continue;
        };
        commands.entity(visual.entity).despawn();
        visual.entity = entity;
        visual.model_loaded = true;
    }
}
fn present(time: Res<Time>, drops: Res<Drops>, mut transforms: Query<&mut Transform>) {
    let seconds = time.elapsed_secs();
    for item in drops.items.values() {
        if let Ok(mut transform) = transforms.get_mut(item.entity) {
            let alpha = (item.received.elapsed().as_secs_f32() / SNAPSHOT_SECONDS).clamp(0.0, 1.0);
            let bob = (seconds * 2.0 + item.phase).sin() * 0.06;
            transform.translation = item.previous.lerp(item.current, alpha) + Vec3::Y * bob;
            transform.rotation = Quat::from_rotation_y(seconds * 1.4 + item.phase);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::world::CommandQueue;

    #[test]
    fn snapshots_create_update_and_remove_visuals_without_accepting_old_ticks() {
        let mut world = World::new();
        let mut queue = CommandQueue::default();
        let mut drops = Drops::default();
        let assets = DropAssets {
            mesh: Handle::default(),
            materials: vec![Handle::default(); 6],
            equipment: Handle::default(),
        };
        let packages = crate::package_hud::ServerPackages::default();
        let snapshot = DropSnapshot {
            id: 7,
            item: 3,
            count: 1,
            position: Vec3::ONE,
        };
        drops.receive(
            1,
            &[snapshot],
            &mut Commands::new(&mut queue, &world),
            &assets,
            &packages,
            Instant::now(),
        );
        queue.apply(&mut world);
        let entity = drops.items[&7].entity;
        assert!(world.get_entity(entity).is_ok());

        // A repeated tick is ignored, so the visual survives an empty duplicate.
        drops.receive(
            1,
            &[],
            &mut Commands::new(&mut queue, &world),
            &assets,
            &packages,
            Instant::now(),
        );
        queue.apply(&mut world);
        assert_eq!(drops.items.len(), 1);

        // A newer tick moves the visual and drops invalid items.
        let invalid = DropSnapshot {
            id: 9,
            item: 0,
            count: 4,
            position: Vec3::ZERO,
        };
        let moved = DropSnapshot {
            position: Vec3::splat(2.0),
            ..snapshot
        };
        drops.receive(
            2,
            &[moved, invalid],
            &mut Commands::new(&mut queue, &world),
            &assets,
            &packages,
            Instant::now(),
        );
        queue.apply(&mut world);
        assert_eq!(drops.items.len(), 1);
        assert_eq!(drops.items[&7].current, Vec3::splat(2.0));

        // Removing the item despawns its entity.
        drops.receive(
            3,
            &[],
            &mut Commands::new(&mut queue, &world),
            &assets,
            &packages,
            Instant::now(),
        );
        queue.apply(&mut world);
        assert!(drops.items.is_empty());
        assert!(world.get_entity(entity).is_err());
    }
}
