//! Bow request admission, projectile replication, and authoritative blast transactions.
use super::{
    Simulation,
    actors::{ACTOR_RESPAWN_TICKS, ACTOR_TARGET, SimEntity, SimEvent},
    bow::{Arrow, Flight},
    explosion::{self, Target},
};
use gameplay::combat::SwingTarget;
use glam::{IVec3, Vec3};
use physics::{CollisionShape, PLAYER_MASS, apply_player_impulse};
use protocol::{MAX_ARROWS, ServerMessage};
use voxel_world::{CHUNK_SIZE, VoxelWorld};

impl Simulation {
    /// Spawn a projectile from a ranged basic attack or ability. `blast: None`
    /// makes a plain arrow that damages the character it strikes directly.
    /// Returns false when the arrow budget is full.
    pub(super) fn spawn_projectile(
        &mut self,
        origin: Vec3,
        direction: Vec3,
        projectile: game_packages::ProjectileSpec,
        blast: Option<game_packages::BlastSpec>,
        damage: u16,
        knockback: f32,
        shooter: Option<SimEntity>,
    ) -> bool {
        if self.arrows.len() + self.detonations.len() >= MAX_ARROWS || self.next_arrow == u32::MAX {
            return false;
        }
        let mut arrow = Arrow::new_arrow(self.next_arrow, origin, direction, projectile, blast);
        arrow.damage = damage;
        arrow.knockback = knockback;
        arrow.shooter = shooter;
        self.arrows.push(arrow);
        self.next_arrow += 1;
        self.arrow_revision += 1;
        true
    }

    /// Every living character as a swing target, for projectile and melee hits.
    /// `exclude` drops the attacker's own target so a swing or arrow cannot hit
    /// the character that produced it.
    pub(super) fn swing_targets(&self, exclude: Option<u64>) -> Vec<SwingTarget> {
        let mut targets: Vec<SwingTarget> = self
            .actors
            .iter()
            .filter(|(_, actor)| !actor.health.is_depleted())
            .map(|(&id, actor)| SwingTarget {
                id: u64::from(id) | ACTOR_TARGET,
                position: actor.state.motion.position,
                shape: actor.body.shape,
            })
            .collect();
        targets.extend(
            self.players
                .iter()
                .filter(|(_, player)| !player.health.is_depleted() && !player.state.motion.noclip)
                .map(|(&id, player)| SwingTarget {
                    id,
                    position: player.state.motion.position,
                    shape: CollisionShape::default(),
                }),
        );
        targets.retain(|target| Some(target.id) != exclude);
        targets
    }

    /// Apply a plain arrow's direct hit: damage plus a knockback impulse along
    /// the flight direction, attributed to the shooter.
    fn apply_arrow_hit(
        &mut self,
        target: u64,
        damage: u16,
        knockback: f32,
        velocity: Vec3,
        shooter: Option<SimEntity>,
    ) {
        let impulse = velocity.try_normalize().unwrap_or(Vec3::NEG_Z) * knockback;
        let tick = self.tick;
        if target & ACTOR_TARGET != 0 {
            let actor_id = (target & !ACTOR_TARGET) as u32;
            let Some(actor) = self.actors.get_mut(&actor_id) else {
                return;
            };
            actor.state.apply_impulse(&actor.body, impulse);
            let was_depleted = actor.health.is_depleted();
            let amount = actor.health.damage(damage);
            let killed = actor.health.is_depleted();
            if killed {
                actor.respawn_at = Some(tick + ACTOR_RESPAWN_TICKS);
            }
            if let Some(source) = shooter {
                self.push_event(SimEvent::DamageDealt {
                    source,
                    target: SimEntity::Actor(actor_id),
                    amount,
                    killed,
                });
            }
            if !was_depleted && killed {
                self.push_event(SimEvent::ActorDied {
                    id: actor_id,
                    killer: shooter,
                });
            }
        } else {
            if let Some(victim) = self.players.get_mut(&target) {
                apply_player_impulse(&mut victim.state.motion, impulse);
            }
            if let Some(source) = shooter {
                self.damage_player_event(source, target, damage);
            }
        }
    }

    pub(super) fn advance_bow(&mut self, world: &mut VoxelWorld) {
        let bodies = self
            .physics
            .as_ref()
            .filter(|p| !p.failed())
            .map(|p| p.snapshots())
            .unwrap_or_default();
        let ready = self
            .physics
            .as_ref()
            .is_none_or(|p| !p.is_busy() || p.failed());
        let arrows = std::mem::take(&mut self.arrows);
        if !arrows.is_empty() {
            self.arrow_revision += 1;
        }
        for mut arrow in arrows {
            let targets = self.swing_targets(arrow.shooter.map(SimEntity::target_id));
            match arrow.tick(world, &bodies, &targets, ready) {
                Flight::Flying => self.arrows.push(arrow),
                Flight::Impact(position, target) => {
                    if let Some(blast) = arrow.blast {
                        // An explosive arrow detonates on any impact.
                        self.detonations
                            .push_back((arrow.snapshot.id, position, blast, arrow.shooter));
                    } else if let Some(target) = target {
                        // A plain arrow damages the character it struck directly.
                        self.apply_arrow_hit(
                            target,
                            arrow.damage,
                            arrow.knockback,
                            arrow.snapshot.velocity,
                            arrow.shooter,
                        );
                    }
                }
                Flight::Expired => {}
            }
        }
        self.detonate_ready(world);
        if self.tick.is_multiple_of(3) {
            let recipients: Vec<_> = self
                .players
                .iter()
                .filter(|(_, player)| player.arrow_revision != Some(self.arrow_revision))
                .map(|(&id, _)| id)
                .collect();
            let message = ServerMessage::Projectiles {
                tick: self.arrow_revision,
                arrows: self.arrows.iter().map(|arrow| arrow.snapshot).collect(),
            };
            for id in recipients {
                if self.send(id, &message) {
                    self.players.get_mut(&id).unwrap().arrow_revision = Some(self.arrow_revision);
                }
            }
        }
    }

    fn detonate_ready(&mut self, world: &mut VoxelWorld) {
        // Never mutate or overwrite GPU-owned bodies using an in-flight observation.
        if self
            .physics
            .as_ref()
            .is_some_and(|p| p.is_busy() && !p.failed())
        {
            return;
        }
        let mut physics = self.physics.take();
        // Bound voxel scans, transactions, and replication work per server tick.
        for _ in 0..2 {
            let Some((id, position, blast, source)) = self.detonations.pop_front() else {
                break;
            };
            let bodies = physics
                .as_ref()
                .filter(|p| !p.failed())
                .map(|p| p.snapshots())
                .unwrap_or_default();
            let mut players: Vec<_> = self
                .players
                .iter()
                .filter(|(_, player)| !player.health.is_depleted())
                .map(|(&id, player)| (id, player.state.motion))
                .collect();
            players.sort_unstable_by_key(|(id, _)| *id);
            let loads = explosion::plan_blast(world, &bodies, &players, position, blast);
            for load in loads {
                match load.target {
                    // Terrain takes one smooth carve below; per-cell fracture
                    // and release loads are superseded by the crater.
                    Target::Grid(_) => {}
                    Target::Player(player_id) => {
                        if let Some(source) = source {
                            self.damage_player_event(source, player_id, load.player_damage);
                        } else {
                            self.damage_player(player_id, load.player_damage);
                        }
                        if let Some(player) = self.players.get_mut(&player_id)
                            && !player.health.is_depleted()
                        {
                            let impulse = explosion::kinetic_impulse(
                                PLAYER_MASS,
                                player.state.motion.velocity,
                                load.direction,
                                load.kinetic_energy,
                            );
                            apply_player_impulse(&mut player.state.motion, impulse);
                        }
                    }
                    Target::Body(body_id) => {
                        if let Some(physics) = &mut physics
                            && let Some((target, material)) = physics.apply_blast(body_id, &load)
                        {
                            self.destroyed(target, material, Some(body_id));
                        }
                    }
                }
            }
            // The crater replaces per-cell detach: carve once, then launch the
            // carved mass as a single debris body. The ejection direction is
            // the pre-carve surface normal, biased upward.
            let outward = world
                .density_gradient(position)
                .and_then(|gradient| (Vec3::Y - gradient).try_normalize())
                .unwrap_or(Vec3::Y);
            if self.has_journal_space(position.floor().as_ivec3()) {
                let crater = (blast.radius * 0.6).clamp(1.5, 2.5);
                let edits = world.brush_dig(position, crater);
                let carved: Vec<IVec3> = edits
                    .iter()
                    .flat_map(|edit| {
                        edit.voxels.iter().map(|&(cell, _, _)| {
                            let cell = i32::from(cell);
                            edit.coord * CHUNK_SIZE
                                + IVec3::new(cell % 32, cell / 1024, (cell / 32) % 32)
                        })
                    })
                    .collect();
                let material = self.record_brush(edits);
                if let Some(physics) = &mut physics {
                    for cell in carved {
                        physics.set_voxel(cell, 0);
                    }
                    let origin = position.floor().as_ivec3();
                    if let Some(material) = material
                        && physics.can_detach(origin)
                    {
                        let mass = gpu_physics::material(material as u32).density;
                        // A small share of the blast budget launches the debris.
                        let impulse = explosion::kinetic_impulse(
                            mass,
                            Vec3::ZERO,
                            outward,
                            blast.energy * 0.05,
                        );
                        physics.release_at(position, origin, material, 0.0, impulse);
                    }
                }
            }
            self.metrics.explosions += 1;
            let event = ServerMessage::Explosion {
                id,
                position,
                radius: blast.radius,
            };
            let recipients: Vec<_> = self.players.keys().copied().collect();
            for recipient in recipients {
                self.send(recipient, &event);
            }
        }
        self.physics = physics;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Player, ServerConfig, SimulationPlugin};
    use bevy_app::App;
    use glam::IVec3;
    use protocol::BowPower;
    use voxel_world::Chunk;

    /// Drive one player's full basic-attack cycle and resolve the shot it
    /// fired, mirroring the server tick's motor step and effect resolution.
    /// The bow charges while held and fires on the release edge at full draw.
    fn fire(sim: &mut Simulation, world: &VoxelWorld, id: u64) {
        let attack = crate::basic_attack_for(&sim.packages, sim.players[&id].input.selected);
        let charge = attack.charge_ticks.max(1);
        for held in std::iter::repeat(true)
            .take(usize::from(charge))
            .chain([false])
        {
            let player = sim.players.get_mut(&id).unwrap();
            player.input.attack = held;
            let output = controller::step_character_player(
                world,
                &mut player.state,
                Some(attack),
                &player.input,
                physics::FIXED_DT,
                &[],
            );
            if output.basic_attack || output.charge_release.is_some() {
                sim.resolve_attacks(world, &[id]);
            }
        }
    }

    #[test]
    fn own_ground_shot_launches_player_once_and_preserves_momentum() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
        for x in 6..=14 {
            for z in 6..=14 {
                world.set_block(IVec3::new(x, 9, z), 3).unwrap();
            }
        }
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        let mut player = Player::new();
        player.state.motion.position = Vec3::new(10.5, 10.0, 10.5);
        player.state.motion.grounded = true;
        player.input.selected = protocol::EXPLOSIVE_BOW_ITEM;
        player.input.attack = true;
        player.input.yaw = 0.0;
        player.input.pitch = -1.2;
        sim.players.insert(1, player);
        fire(&mut sim, &world, 1);
        for _ in 0..10 {
            sim.advance_bow(&mut world);
        }
        assert_eq!(sim.metrics.explosions, 1);
        let health = sim.player_health(1).unwrap();
        assert!(health.current() > 0 && health.current() < health.maximum());
        let launched = sim.players[&1].state;
        assert!(launched.motion.velocity.y > 2.0, "{launched:?}");
        assert!(launched.motion.external_velocity.y > 0.0);
        assert!(!launched.motion.grounded);
        sim.advance_bow(&mut world);
        assert_eq!(sim.players[&1].state, launched);
        assert_eq!(sim.player_health(1), Some(health));
        let mut after = launched;
        controller::step_player(&world, &mut after.motion, &Default::default(), physics::FIXED_DT);
        assert!(after.motion.position.y > launched.motion.position.y);
        assert!(after.motion.position.z > launched.motion.position.z);
    }

    #[test]
    fn queued_blasts_accumulate_on_players_without_reapplying_drained_events() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        let mut player = Player::new();
        player.state.motion.position = Vec3::new(10.5, 10.0, 10.5);
        let center = player.state.motion.position;
        sim.players.insert(1, player);
        for id in 0..3 {
            sim.detonations.push_back((id, center, crate::packages::test_blast(BowPower::Standard), None));
        }
        sim.detonate_ready(&mut world);
        assert_eq!(sim.detonations.len(), 1);
        let first_speed = sim.players[&1].state.motion.velocity.y;
        sim.detonate_ready(&mut world);
        let final_state = sim.players[&1].state;
        assert!(first_speed > 0.0);
        assert!(sim.player_health(1).unwrap().is_depleted());
        assert_eq!(final_state.motion.velocity, Vec3::ZERO);
        assert_eq!(final_state.motion.external_velocity, glam::Vec2::ZERO);
        assert_eq!(sim.metrics.explosions, 3);
        sim.detonate_ready(&mut world);
        assert_eq!(sim.players[&1].state, final_state);
    }

    #[test]
    fn blast_damages_each_exposed_player_once_using_the_pre_destruction_world() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
        let center = Vec3::new(10.5, 10.9, 10.5);
        // This wall is destroyed, but still shields player 3 from this blast.
        for y in 9..=12 {
            world.set_block(IVec3::new(11, y, 10), 1).unwrap();
        }
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        for (id, position, noclip) in [
            (1, Vec3::new(9.5, 10.0, 10.5), false),
            (2, Vec3::new(9.5, 10.0, 10.5), false),
            (3, Vec3::new(12.5, 10.0, 10.5), false),
            (4, Vec3::new(9.5, 10.0, 10.5), true),
            (5, Vec3::new(20.5, 10.0, 10.5), false),
        ] {
            let mut player = Player::new();
            player.state.motion.position = position;
            player.state.motion.noclip = noclip;
            sim.players.insert(id, player);
        }
        let expected = explosion::plan(
            &world,
            &[],
            &[(1, sim.players[&1].state.motion)],
            center,
            BowPower::Standard,
        )
        .into_iter()
        .find(|load| load.target == Target::Player(1))
        .unwrap()
        .player_damage;
        sim.detonations
            .push_back((1, center, crate::packages::test_blast(BowPower::Standard), None));
        sim.detonate_ready(&mut world);
        assert_eq!(sim.player_health(1).unwrap().current(), 100 - expected);
        assert_eq!(sim.player_health(2), sim.player_health(1));
        assert!(expected > 0);
        assert!((9..=12).any(|y| world.block(IVec3::new(11, y, 10)) == Some(0)));
        for id in [3, 4, 5] {
            assert_eq!(sim.player_health(id).unwrap().current(), 100);
        }
        sim.detonate_ready(&mut world);
        assert_eq!(sim.player_health(1).unwrap().current(), 100 - expected);
    }

    #[test]
    fn dead_players_cannot_fire_or_receive_another_impulse() {
        let mut app = App::new();
        app.add_plugins(
            SimulationPlugin::headless(ServerConfig {
                spawn_titan: false,
                ..Default::default()
            })
            .unwrap(),
        );
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
        {
            let mut sim = app.world_mut().resource_mut::<Simulation>();
            let mut player = Player::new();
            player.state.motion.position = Vec3::splat(10.0);
            player.input.selected = protocol::EXPLOSIVE_BOW_ITEM;
            player.input.attack = true;
            sim.players.insert(1, player);
            sim.damage_player(1, 100);
        }
        app.insert_resource(world);
        // The tick drops a dead player's attack input before `resolve_attacks`.
        app.update();
        let mut world = app.world_mut().remove_resource::<VoxelWorld>().unwrap();
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        assert!(sim.arrows.is_empty());
        sim.detonations.push_back((
            1,
            Vec3::splat(10.0),
            crate::packages::test_blast(BowPower::Standard),
            None,
        ));
        sim.detonate_ready(&mut world);
        assert_eq!(sim.players[&1].state.motion.velocity, Vec3::ZERO);
        assert_eq!(sim.players[&1].state.motion.external_velocity, glam::Vec2::ZERO);
        assert!(sim.player_health(1).unwrap().is_depleted());
    }

    #[test]
    fn detonation_carves_a_smooth_crater_and_journals_every_voxel() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
        for x in 6..=14 {
            for z in 6..=14 {
                for y in 7..=9 {
                    world
                        .set_voxel(IVec3::new(x, y, z), voxel_world::Voxel::terrain(3))
                        .unwrap();
                }
            }
        }
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        let mut player = Player::new();
        player.state.motion.position = Vec3::new(10.5, 20.0, 10.5);
        player
            .known
            .insert(IVec3::ZERO, world.chunks[&IVec3::ZERO].revision);
        sim.players.insert(1, player);
        sim.detonations.push_back((
            1,
            Vec3::new(10.5, 10.0, 10.5),
            crate::packages::test_blast(BowPower::Standard),
            None,
        ));
        sim.detonate_ready(&mut world);
        assert_eq!(sim.metrics.explosions, 1);
        // The crater is smooth: the core is carved to air, the rim feathers.
        assert_eq!(world.block(IVec3::new(10, 9, 10)), Some(0));
        assert!(
            (6..=14).any(|x| (6..=14).any(|z| {
                world
                    .density(IVec3::new(x, 9, z))
                    .is_some_and(|d| d < 127 && d > -128)
            })),
            "blast left no partially carved voxel"
        );
        // Every carved cell is journaled, and the delta advanced the player's
        // known revision to the journal tip.
        let journal = &sim.journal[&IVec3::ZERO];
        assert!(journal.voxels.len() > 1);
        assert_eq!(sim.players[&1].known[&IVec3::ZERO], journal.revision);
        assert_eq!(world.chunks[&IVec3::ZERO].revision, journal.revision);
    }
}
