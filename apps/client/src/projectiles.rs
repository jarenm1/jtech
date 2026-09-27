//! Bounded presentation of server projectiles and detonations. No local terrain changes.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Instant,
};

use bevy::prelude::*;
use protocol::{MAX_PROJECTILES, ProjectileSnapshot};

const SNAPSHOT_SECONDS: f32 = 0.05;
const PROJECTILE_TIMEOUT: f32 = 1.0;
const BLAST_SECONDS: f32 = 0.45;
const MAX_BLASTS: usize = 32;
const FADE_STEPS: usize = 16;
const RECENT_EXPLOSIONS: usize = 256;

pub struct ProjectilesPlugin;
impl Plugin for ProjectilesPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Projectiles>()
            .add_systems(Startup, setup)
            .add_systems(Update, present.after(super::edit_blocks));
    }
}

struct ProjectileVisual {
    entity: Entity,
    previous: Vec3,
    current: Vec3,
    rotation: Quat,
    received: Instant,
}
impl ProjectileVisual {
    fn position(&self, now: Instant) -> Vec3 {
        self.previous.lerp(
            self.current,
            (now.duration_since(self.received).as_secs_f32() / SNAPSHOT_SECONDS).clamp(0.0, 1.0),
        )
    }
}

struct BlastVisual {
    entity: Entity,
    radius: f32,
    born: Instant,
}

#[derive(Resource, Default)]
pub struct Projectiles {
    projectiles: HashMap<u32, ProjectileVisual>,
    tick: Option<u64>,
    blasts: VecDeque<BlastVisual>,
    seen: HashSet<u32>,
    recent: VecDeque<u32>,
}

#[derive(Resource)]
pub struct ProjectileAssets {
    shaft: Handle<Mesh>,
    tip: Handle<Mesh>,
    sphere: Handle<Mesh>,
    shaft_material: Handle<StandardMaterial>,
    tip_material: Handle<StandardMaterial>,
    blast_materials: Vec<Handle<StandardMaterial>>,
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.insert_resource(ProjectileAssets {
        shaft: meshes.add(Cuboid::new(0.045, 0.045, 0.8)),
        tip: meshes.add(Sphere::new(0.09)),
        sphere: meshes.add(Sphere::new(1.0)),
        shaft_material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.75, 0.42, 0.12),
            unlit: true,
            ..default()
        }),
        tip_material: materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.9, 0.3),
            unlit: true,
            ..default()
        }),
        // Shared fade palette: resource counts are independent of projectiles fired.
        blast_materials: (0..FADE_STEPS)
            .map(|step| {
                let life = step as f32 / (FADE_STEPS - 1) as f32;
                materials.add(StandardMaterial {
                    base_color: Color::srgba(1.0, 0.65 - life * 0.4, 0.08, 0.5 * (1.0 - life)),
                    alpha_mode: AlphaMode::Blend,
                    unlit: true,
                    cull_mode: None,
                    ..default()
                })
            })
            .collect(),
    });
}

fn orientation(velocity: Vec3) -> Quat {
    velocity
        .try_normalize()
        .map_or(Quat::IDENTITY, |direction| {
            Quat::from_rotation_arc(Vec3::Z, direction)
        })
}

impl Projectiles {
    pub fn clear(&mut self, commands: &mut Commands) {
        for (_, projectile) in self.projectiles.drain() {
            commands.entity(projectile.entity).despawn();
        }
        for blast in self.blasts.drain(..) {
            commands.entity(blast.entity).despawn();
        }
        self.tick = None;
        self.seen.clear();
        self.recent.clear();
    }

    pub fn receive(
        &mut self,
        tick: u64,
        projectiles: &[ProjectileSnapshot],
        commands: &mut Commands,
        assets: &ProjectileAssets,
        now: Instant,
    ) {
        if self.tick.is_some_and(|old| tick <= old) {
            return;
        }
        self.tick = Some(tick);
        let valid: Vec<_> = projectiles
            .iter()
            .take(MAX_PROJECTILES)
            .filter(|projectile| {
                projectile.position.is_finite()
                    && projectile.velocity.is_finite()
                    && !self.seen.contains(&projectile.id)
            })
            .collect();
        let ids: HashSet<_> = valid.iter().map(|projectile| projectile.id).collect();
        self.projectiles.retain(|id, projectile| {
            if ids.contains(id) {
                true
            } else {
                commands.entity(projectile.entity).despawn();
                false
            }
        });
        for projectile in valid {
            let rotation = orientation(projectile.velocity);
            self.projectiles
                .entry(projectile.id)
                .and_modify(|visual| {
                    visual.previous = visual.position(now);
                    visual.current = projectile.position;
                    visual.rotation = rotation;
                    visual.received = now;
                })
                .or_insert_with(|| {
                    let entity = commands
                        .spawn((
                            Mesh3d(assets.shaft.clone()),
                            MeshMaterial3d(assets.shaft_material.clone()),
                            Transform::from_translation(projectile.position).with_rotation(rotation),
                        ))
                        .with_children(|parent| {
                            parent.spawn((
                                Mesh3d(assets.tip.clone()),
                                MeshMaterial3d(assets.tip_material.clone()),
                                Transform::from_xyz(0.0, 0.0, 0.4),
                            ));
                        })
                        .id();
                    ProjectileVisual {
                        entity,
                        previous: projectile.position,
                        current: projectile.position,
                        rotation,
                        received: now,
                    }
                });
        }
    }

    #[allow(clippy::too_many_arguments)] // One server event plus rendering context.
    pub fn explode(
        &mut self,
        id: u32,
        position: Vec3,
        radius: f32,
        commands: &mut Commands,
        assets: &ProjectileAssets,
        now: Instant,
    ) {
        if !position.is_finite() || !radius.is_finite() || radius <= 0.0 || !self.seen.insert(id) {
            return;
        }
        self.recent.push_back(id);
        if self.recent.len() > RECENT_EXPLOSIONS {
            self.seen.remove(&self.recent.pop_front().unwrap());
        }
        if let Some(projectile) = self.projectiles.remove(&id) {
            commands.entity(projectile.entity).despawn();
        }
        if self.blasts.len() == MAX_BLASTS {
            commands
                .entity(self.blasts.pop_front().unwrap().entity)
                .despawn();
        }
        let entity = commands
            .spawn((
                Mesh3d(assets.sphere.clone()),
                MeshMaterial3d(assets.blast_materials[0].clone()),
                Transform::from_translation(position).with_scale(Vec3::splat(0.1)),
            ))
            .id();
        self.blasts.push_back(BlastVisual {
            entity,
            radius: radius.min(64.0),
            born: now,
        });
    }

    fn expire(&mut self, commands: &mut Commands, now: Instant) {
        self.projectiles.retain(|_, projectile| {
            if now.duration_since(projectile.received).as_secs_f32() < PROJECTILE_TIMEOUT {
                true
            } else {
                commands.entity(projectile.entity).despawn();
                false
            }
        });
        self.blasts.retain(|blast| {
            if now.duration_since(blast.born).as_secs_f32() < BLAST_SECONDS {
                true
            } else {
                commands.entity(blast.entity).despawn();
                false
            }
        });
    }
}

fn present(
    mut commands: Commands,
    session: Res<super::ClientSession>,
    mut projectiles: ResMut<Projectiles>,
    assets: Res<ProjectileAssets>,
    mut visuals: Query<(&mut Transform, &mut MeshMaterial3d<StandardMaterial>)>,
) {
    if session.transport.is_none() {
        projectiles.clear(&mut commands);
        return;
    }
    let now = Instant::now();
    projectiles.expire(&mut commands, now);
    for projectile in projectiles.projectiles.values() {
        if let Ok((mut transform, _)) = visuals.get_mut(projectile.entity) {
            transform.translation = projectile.position(now);
            transform.rotation = projectile.rotation;
        }
    }
    for blast in &projectiles.blasts {
        if let Ok((mut transform, mut material)) = visuals.get_mut(blast.entity) {
            let life =
                (now.duration_since(blast.born).as_secs_f32() / BLAST_SECONDS).clamp(0.0, 1.0);
            transform.scale = Vec3::splat(blast.radius * (0.05 + 0.95 * life.sqrt()));
            material.0 = assets.blast_materials[(life * (FADE_STEPS - 1) as f32) as usize].clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::world::CommandQueue;
    use std::time::Duration;

    fn assets() -> ProjectileAssets {
        ProjectileAssets {
            shaft: Handle::default(),
            tip: Handle::default(),
            sphere: Handle::default(),
            shaft_material: Handle::default(),
            tip_material: Handle::default(),
            blast_materials: vec![Handle::default(); FADE_STEPS],
        }
    }

    #[test]
    fn snapshot_lifecycle_interpolation_and_stale_ticks() {
        let mut world = World::new();
        let mut queue = CommandQueue::default();
        let mut state = Projectiles::default();
        let assets = assets();
        let now = Instant::now();
        let mut projectile = ProjectileSnapshot {
            id: 7,
            position: Vec3::ZERO,
            velocity: Vec3::X * 36.0,
        };
        state.receive(
            1,
            &[projectile],
            &mut Commands::new(&mut queue, &world),
            &assets,
            now,
        );
        queue.apply(&mut world);
        let entity = state.projectiles[&7].entity;
        assert_eq!(world.entities().len(), 2); // Shaft and linked tip.
        assert!((state.projectiles[&7].rotation * Vec3::Z - Vec3::X).length() < 0.001);
        projectile.position = Vec3::X * 2.0;
        state.receive(
            2,
            &[projectile],
            &mut Commands::new(&mut queue, &world),
            &assets,
            now,
        );
        assert!((state.projectiles[&7].position(now + Duration::from_millis(25)).x - 1.0).abs() < 0.001);
        state.receive(1, &[], &mut Commands::new(&mut queue, &world), &assets, now);
        assert_eq!(state.projectiles[&7].entity, entity);
        state.receive(3, &[], &mut Commands::new(&mut queue, &world), &assets, now);
        queue.apply(&mut world);
        assert_eq!(world.entities().len(), 0);
    }

    #[test]
    fn effects_deduplicate_expire_bound_entities_and_reset_session() {
        let mut world = World::new();
        let mut queue = CommandQueue::default();
        let mut state = Projectiles::default();
        let assets = assets();
        let now = Instant::now();
        let projectile = ProjectileSnapshot {
            id: 7,
            position: Vec3::ZERO,
            velocity: Vec3::Z,
        };
        state.receive(
            1,
            &[projectile],
            &mut Commands::new(&mut queue, &world),
            &assets,
            now,
        );
        for _ in 0..2 {
            state.explode(
                7,
                Vec3::ZERO,
                4.0,
                &mut Commands::new(&mut queue, &world),
                &assets,
                now,
            );
        }
        state.receive(
            2,
            &[projectile],
            &mut Commands::new(&mut queue, &world),
            &assets,
            now,
        );
        queue.apply(&mut world);
        assert_eq!(world.entities().len(), 1);
        assert!(state.projectiles.is_empty());
        for id in 8..300 {
            state.explode(
                id,
                Vec3::ZERO,
                4.0,
                &mut Commands::new(&mut queue, &world),
                &assets,
                now,
            );
        }
        queue.apply(&mut world);
        assert_eq!(world.entities().len(), MAX_BLASTS as u32);
        assert_eq!(state.seen.len(), RECENT_EXPLOSIONS);
        state.expire(
            &mut Commands::new(&mut queue, &world),
            now + Duration::from_secs(2),
        );
        queue.apply(&mut world);
        assert_eq!(world.entities().len(), 0);
        state.clear(&mut Commands::new(&mut queue, &world));
        state.receive(
            0,
            &[projectile],
            &mut Commands::new(&mut queue, &world),
            &assets,
            now,
        );
        queue.apply(&mut world);
        assert_eq!(state.projectiles.len(), 1);
        state.expire(
            &mut Commands::new(&mut queue, &world),
            now + Duration::from_secs(2),
        );
        queue.apply(&mut world);
        assert_eq!(world.entities().len(), 0);
    }

    #[test]
    fn disconnect_clears_live_entities_and_session_history() {
        let mut app = App::new();
        let assets = assets();
        let mut state = Projectiles::default();
        let mut queue = CommandQueue::default();
        let now = Instant::now();
        state.receive(
            9,
            &[ProjectileSnapshot {
                id: 1,
                position: Vec3::ZERO,
                velocity: Vec3::Z,
            }],
            &mut Commands::new(&mut queue, app.world()),
            &assets,
            now,
        );
        state.explode(
            2,
            Vec3::ZERO,
            4.0,
            &mut Commands::new(&mut queue, app.world()),
            &assets,
            now,
        );
        queue.apply(app.world_mut());
        assert_eq!(app.world().entities().len(), 3);
        app.insert_resource(state)
            .insert_resource(assets)
            .init_resource::<super::super::ClientSession>()
            .add_systems(Update, present);
        app.update();
        assert_eq!(app.world().entities().len(), 0);
        let state = app.world().resource::<Projectiles>();
        assert!(state.projectiles.is_empty() && state.blasts.is_empty() && state.seen.is_empty());
        assert_eq!(state.tick, None);
    }

    #[test]
    fn snapshots_are_bounded_and_invalid_coordinates_are_ignored() {
        let mut world = World::new();
        let mut queue = CommandQueue::default();
        let mut state = Projectiles::default();
        let assets = assets();
        let now = Instant::now();
        let mut projectiles: Vec<_> = (0..MAX_PROJECTILES as u32 + 4)
            .map(|id| ProjectileSnapshot {
                id,
                position: Vec3::ZERO,
                velocity: Vec3::ZERO,
            })
            .collect();
        projectiles[0].position.x = f32::NAN;
        state.receive(
            0,
            &projectiles,
            &mut Commands::new(&mut queue, &world),
            &assets,
            now,
        );
        queue.apply(&mut world);
        assert_eq!(state.projectiles.len(), MAX_PROJECTILES - 1);
        assert_eq!(world.entities().len(), 2 * (MAX_PROJECTILES as u32 - 1));
        state.explode(
            99,
            Vec3::ZERO,
            f32::NAN,
            &mut Commands::new(&mut queue, &world),
            &assets,
            now,
        );
        assert!(state.blasts.is_empty() && state.seen.is_empty());
        state.clear(&mut Commands::new(&mut queue, &world));
        queue.apply(&mut world);
        assert_eq!(world.entities().len(), 0);
    }
}
