//! Presentation of authoritative GPU bodies. Clients interpolate; they do not solve physics.
use std::{collections::HashMap, time::Instant};

use bevy::prelude::*;
use protocol::PhysicsBodySnapshot;

pub struct LooseBlocksPlugin;
impl Plugin for LooseBlocksPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LooseBlocks>()
            .add_systems(Startup, setup)
            .add_systems(Update, interpolate.after(super::receive_network));
    }
}

struct BodyVisual {
    entity: Entity,
    previous: Vec3,
    current: Vec3,
    received: Instant,
}

#[derive(Resource, Default)]
pub struct LooseBlocks {
    bodies: HashMap<u32, BodyVisual>,
    tick: Option<u64>,
    colliders: Vec<physics::DynamicCollider>,
}

#[derive(Resource)]
pub struct LooseBlockAssets {
    mesh: Handle<Mesh>,
    materials: Vec<Handle<StandardMaterial>>,
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.insert_resource(LooseBlockAssets {
        mesh: meshes.add(Cuboid::from_length(1.0)),
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
    });
}

impl LooseBlocks {
    pub fn colliders(&self) -> &[physics::DynamicCollider] {
        &self.colliders
    }
    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.bodies.len()
    }

    pub fn receive(
        &mut self,
        tick: u64,
        bodies: &[PhysicsBodySnapshot],
        commands: &mut Commands,
        assets: &LooseBlockAssets,
    ) {
        if self.tick.is_some_and(|old| tick <= old) {
            return;
        }
        self.tick = Some(tick);
        self.colliders = bodies
            .iter()
            .filter(|body| {
                body.position.is_finite()
                    && body.velocity.is_finite()
                    && body.material > 0
                    && body.material <= voxel_world::WOOD
            })
            .map(|body| physics::DynamicCollider {
                id: body.id,
                position: body.position,
                velocity: body.velocity,
            })
            .collect();
        self.colliders.sort_unstable_by_key(|body| body.id);
        let now = Instant::now();
        let ids: std::collections::HashSet<_> = bodies.iter().map(|body| body.id).collect();
        self.bodies.retain(|id, visual| {
            if ids.contains(id) {
                true
            } else {
                commands.entity(visual.entity).despawn();
                false
            }
        });
        for body in bodies {
            if !body.position.is_finite() || body.material == voxel_world::AIR {
                continue;
            }
            let Some(material) = assets.materials.get(body.material as usize) else {
                continue;
            };
            self.bodies
                .entry(body.id)
                .and_modify(|visual| {
                    // Continue from the displayed position if snapshots arrive in a burst.
                    let alpha = (visual.received.elapsed().as_secs_f32() / 0.05).clamp(0.0, 1.0);
                    visual.previous = visual.previous.lerp(visual.current, alpha);
                    visual.current = body.position;
                    visual.received = now;
                })
                .or_insert_with(|| BodyVisual {
                    entity: commands
                        .spawn((
                            Mesh3d(assets.mesh.clone()),
                            MeshMaterial3d(material.clone()),
                            Transform::from_translation(body.position),
                        ))
                        .id(),
                    previous: body.position,
                    current: body.position,
                    received: now,
                });
        }
    }
}

fn interpolate(bodies: Res<LooseBlocks>, mut transforms: Query<&mut Transform>) {
    for body in bodies.bodies.values() {
        if let Ok(mut transform) = transforms.get_mut(body.entity) {
            let alpha = (body.received.elapsed().as_secs_f32() / 0.05).clamp(0.0, 1.0);
            transform.translation = body.previous.lerp(body.current, alpha);
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
        let mut bodies = LooseBlocks::default();
        let assets = LooseBlockAssets {
            mesh: Handle::default(),
            materials: vec![Handle::default(); 6],
        };
        let snapshot = PhysicsBodySnapshot {
            id: 7,
            position: Vec3::ONE,
            velocity: Vec3::ZERO,
            material: 3,
        };
        bodies.receive(
            1,
            &[snapshot],
            &mut Commands::new(&mut queue, &world),
            &assets,
        );
        queue.apply(&mut world);
        let entity = bodies.bodies[&7].entity;
        assert_eq!(
            world.get::<Transform>(entity).unwrap().translation,
            Vec3::ONE
        );
        bodies.receive(
            2,
            &[PhysicsBodySnapshot {
                position: Vec3::splat(2.0),
                ..snapshot
            }],
            &mut Commands::new(&mut queue, &world),
            &assets,
        );
        assert_eq!(bodies.bodies[&7].entity, entity);
        assert_eq!(bodies.bodies[&7].current, Vec3::splat(2.0));
        bodies.receive(1, &[], &mut Commands::new(&mut queue, &world), &assets);
        assert_eq!(bodies.count(), 1);
        bodies.receive(3, &[], &mut Commands::new(&mut queue, &world), &assets);
        queue.apply(&mut world);
        assert_eq!(bodies.count(), 0);
        assert!(world.get_entity(entity).is_err());
    }
}
