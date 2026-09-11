//! Bow request admission, projectile replication, and authoritative blast transactions.
use super::{
    Simulation,
    bow::{Arrow, Flight},
    explosion::{self, Target},
};
use glam::Vec3;
use physics::{EYE_HEIGHT, PLAYER_MASS, apply_player_impulse, look_direction};
use protocol::{BowPower, EditRejection, MAX_ARROWS, ServerMessage};
use voxel_world::VoxelWorld;

impl Simulation {
    pub(super) fn fire_bow(
        &mut self,
        world: &VoxelWorld,
        id: u64,
        request: u64,
        yaw: f32,
        pitch: f32,
        power: BowPower,
    ) {
        let Some(player) = self.players.get(&id) else {
            return;
        };
        if let Some((_, outcome)) = player.results.iter().find(|(old, _)| *old == request) {
            self.send_edit_result(id, request, *outcome);
            return;
        }
        if self
            .strikes
            .iter()
            .any(|s| s.id == id && s.request == request)
        {
            return;
        }
        let origin = player.state.position + Vec3::Y * EYE_HEIGHT;
        let subticks_per_tick = u64::from(self.packages.shots_per_second());
        let now = self.tick.saturating_mul(subticks_per_tick);
        let rejection = if request <= player.highest_request {
            Some(EditRejection::OldRequest)
        } else if player.health.is_depleted() {
            Some(EditRejection::Dead)
        } else if subticks_per_tick == 0 {
            Some(EditRejection::PackageUnavailable)
        } else if now < player.next_bow_time {
            Some(EditRejection::Cooldown)
        } else if !yaw.is_finite()
            || !pitch.is_finite()
            || pitch.abs() > std::f32::consts::FRAC_PI_2
        {
            Some(EditRejection::InvalidTarget)
        } else if world.block(origin.floor().as_ivec3()) != Some(0) {
            Some(EditRejection::Occupied)
        } else if self.arrows.len() + self.detonations.len() >= MAX_ARROWS
            || self.next_arrow == u32::MAX
        {
            Some(EditRejection::BodyCapacity)
        } else if self.physics.as_ref().is_some_and(|p| p.failed()) {
            Some(EditRejection::PhysicsUnavailable)
        } else {
            None
        };
        if let Some(reason) = rejection {
            self.finish_edit(id, request, Err(reason));
            return;
        }
        let shot = match self.packages.fire(power) {
            Ok(shot) => shot,
            Err(_) => {
                self.finish_edit(id, request, Err(EditRejection::PackageUnavailable));
                return;
            }
        };
        self.arrows.push(Arrow::from_shot(
            self.next_arrow,
            origin,
            look_direction(yaw, pitch),
            shot,
        ));
        self.next_arrow += 1;
        self.arrow_revision += 1;
        self.metrics.bow_shots += 1;
        let deadline = &mut self.players.get_mut(&id).unwrap().next_bow_time;
        // Carry sub-tick rounding forward for the package's authored firing rate.
        // After idle time, start a new cadence rather than banking burst shots.
        if now.saturating_sub(*deadline) >= subticks_per_tick {
            *deadline = now;
        }
        *deadline = deadline.saturating_add(60);
        self.finish_edit(id, request, Ok(None));
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
            match arrow.tick(world, &bodies, ready) {
                Flight::Flying => self.arrows.push(arrow),
                Flight::Impact(position) => {
                    self.detonations
                        .push_back((arrow.snapshot.id, position, arrow.shot.impact()))
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
            let Some((id, position, blast)) = self.detonations.pop_front() else {
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
                .map(|(&id, player)| (id, player.state))
                .collect();
            players.sort_unstable_by_key(|(id, _)| *id);
            let loads = explosion::plan_blast(world, &bodies, &players, position, blast);
            for load in loads {
                match load.target {
                    Target::Grid(target) => {
                        if self.apply_contact(world, &load.contact).is_err() {
                            self.metrics.rejected_contacts += 1;
                            continue;
                        }
                        if let Some(physics) = &mut physics {
                            if world.block(target) == Some(0) {
                                physics.set_voxel(target, 0);
                                continue;
                            }
                            // An intact attachment absorbs the reaction in the grid.
                            // Only a successful release may consume reserved motion energy.
                            if let Some(state) = self.damage.get(&target).copied()
                                && state.release
                                && physics.can_detach(target)
                                && self.has_journal_space(target)
                                && let Some((from, to)) = world.set_block(target, 0)
                            {
                                let mass = gpu_physics::material(state.material as u32).density;
                                let impulse = explosion::kinetic_impulse(
                                    mass,
                                    Vec3::ZERO,
                                    load.direction,
                                    load.kinetic_energy,
                                );
                                physics.release(target, state.material, state.joules, impulse);
                                physics.set_voxel(target, 0);
                                self.record_change(world, target, 0, from, to);
                            }
                        }
                    }
                    Target::Player(player_id) => {
                        self.damage_player(player_id, load.player_damage);
                        if let Some(player) = self.players.get_mut(&player_id)
                            && !player.health.is_depleted()
                        {
                            let impulse = explosion::kinetic_impulse(
                                PLAYER_MASS,
                                player.state.velocity,
                                load.direction,
                                load.kinetic_energy,
                            );
                            apply_player_impulse(&mut player.state, impulse);
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
    use voxel_world::Chunk;

    #[test]
    fn bow_admits_25_shots_per_second_without_idle_or_replay_bursts() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        sim.players.insert(1, Player::new());
        let mut request = 0;
        for tick in 0..600 {
            sim.tick = tick;
            // Retire projectiles here to isolate admission from the active-arrow cap.
            sim.arrows.clear();
            request += 1;
            sim.fire_bow(&world, 1, request, 0.0, 0.0, BowPower::Standard);
            let shots = sim.metrics.bow_shots;
            sim.fire_bow(&world, 1, request, 0.0, 0.0, BowPower::Standard);
            assert_eq!(sim.metrics.bow_shots, shots);
            if tick % 60 == 59 {
                assert_eq!(shots, (tick / 60 + 1) * 25);
            }
        }
        sim.tick = 1200;
        sim.arrows.clear();
        request += 1;
        sim.fire_bow(&world, 1, request, 0.0, 0.0, BowPower::Standard);
        assert_eq!(sim.metrics.bow_shots, 251);
        for tick in 1200..1203 {
            sim.tick = tick;
            request += 1;
            sim.fire_bow(&world, 1, request, 0.0, 0.0, BowPower::Standard);
            assert_eq!(sim.metrics.bow_shots, 251);
        }
        sim.tick = 1203;
        sim.fire_bow(&world, 1, request + 1, 0.0, 0.0, BowPower::Standard);
        assert_eq!(sim.metrics.bow_shots, 252);
    }

    #[test]
    fn each_shot_keeps_its_power_through_impact_and_queued_detonation() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
        world.set_block(IVec3::new(5, 10, 4), 3).unwrap();
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        let mut player = Player::new();
        player.state.position = Vec3::new(4.5, 9.0, 4.5);
        sim.players.insert(1, player);
        let yaw = -std::f32::consts::FRAC_PI_2;
        for (index, power) in BowPower::ALL.into_iter().enumerate() {
            sim.tick = index as u64 * 10;
            sim.fire_bow(&world, 1, index as u64 + 1, yaw, 0.0, power);
        }
        assert_eq!(
            sim.arrows
                .iter()
                .map(|arrow| arrow.shot.power)
                .collect::<Vec<_>>(),
            BowPower::ALL
        );
        // A replay with a changed preset must not alter an existing arrow.
        sim.fire_bow(&world, 1, 1, yaw, 0.0, BowPower::Extreme);
        assert_eq!(sim.arrows.len(), 4);
        assert_eq!(sim.arrows[0].shot.power, BowPower::Low);
        // Occupy this tick's two detonation slots, retaining the new impacts in the queue.
        for id in [100, 101] {
            sim.detonations.push_back((
                id,
                Vec3::splat(24.0),
                crate::packages::test_blast(BowPower::Low),
            ));
        }
        sim.advance_bow(&mut world);
        assert!(sim.arrows.is_empty());
        assert_eq!(sim.metrics.explosions, 2);
        assert_eq!(
            sim.detonations
                .iter()
                .map(|(_, _, power)| *power)
                .collect::<Vec<_>>(),
            BowPower::ALL.map(crate::packages::test_blast)
        );
        sim.advance_bow(&mut world);
        assert_eq!(sim.metrics.explosions, 4);
        assert_eq!(sim.detonations.len(), 2);
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
        player.state.position = Vec3::new(10.5, 10.0, 10.5);
        player.state.grounded = true;
        sim.players.insert(1, player);
        sim.fire_bow(&world, 1, 1, 0.0, -1.2, BowPower::Extreme);
        for _ in 0..10 {
            sim.advance_bow(&mut world);
        }
        assert_eq!(sim.metrics.explosions, 1);
        let health = sim.player_health(1).unwrap();
        assert!(health.current() > 0 && health.current() < health.maximum());
        let launched = sim.players[&1].state;
        assert!(launched.velocity.y > 2.0, "{launched:?}");
        assert!(launched.external_velocity.y > 0.0);
        assert!(!launched.grounded);
        sim.advance_bow(&mut world);
        assert_eq!(sim.players[&1].state, launched);
        assert_eq!(sim.player_health(1), Some(health));
        let mut after = launched;
        physics::step_player(&world, &mut after, &Default::default(), physics::FIXED_DT);
        assert!(after.position.y > launched.position.y);
        assert!(after.position.z > launched.position.z);
    }

    #[test]
    fn queued_blasts_accumulate_on_players_without_reapplying_drained_events() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        let mut player = Player::new();
        player.state.position = Vec3::new(10.5, 10.0, 10.5);
        let center = player.state.position;
        sim.players.insert(1, player);
        for id in 0..3 {
            sim.detonations.push_back((
                id,
                center,
                crate::packages::test_blast(BowPower::Standard),
            ));
        }
        sim.detonate_ready(&mut world);
        assert_eq!(sim.detonations.len(), 1);
        let first_speed = sim.players[&1].state.velocity.y;
        sim.detonate_ready(&mut world);
        let final_state = sim.players[&1].state;
        assert!(first_speed > 0.0);
        assert!(sim.player_health(1).unwrap().is_depleted());
        assert_eq!(final_state.velocity, Vec3::ZERO);
        assert_eq!(final_state.external_velocity, glam::Vec2::ZERO);
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
            player.state.position = position;
            player.state.noclip = noclip;
            sim.players.insert(id, player);
        }
        let expected = explosion::plan(
            &world,
            &[],
            &[(1, sim.players[&1].state)],
            center,
            BowPower::Standard,
        )
        .into_iter()
        .find(|load| load.target == Target::Player(1))
        .unwrap()
        .player_damage;
        sim.detonations
            .push_back((1, center, crate::packages::test_blast(BowPower::Standard)));
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
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        let mut player = Player::new();
        player.state.position = Vec3::splat(10.0);
        sim.players.insert(1, player);
        sim.damage_player(1, 100);
        sim.fire_bow(&world, 1, 1, 0.0, 0.0, BowPower::Standard);
        assert_eq!(
            sim.players[&1].results.back().unwrap().1,
            Err(EditRejection::Dead)
        );
        assert!(sim.arrows.is_empty());
        sim.detonations.push_back((
            1,
            Vec3::splat(10.0),
            crate::packages::test_blast(BowPower::Standard),
        ));
        sim.detonate_ready(&mut world);
        assert_eq!(sim.players[&1].state.velocity, Vec3::ZERO);
        assert_eq!(sim.players[&1].state.external_velocity, glam::Vec2::ZERO);
        assert!(sim.player_health(1).unwrap().is_depleted());
    }
}
