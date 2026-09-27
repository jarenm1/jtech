//! Launcher request admission and projectile replication. Packages author the
//! items, presets, and cadence; this module owns the mechanics.
use super::{
    Simulation,
    projectile::{Flight, Projectile},
};
use glam::Vec3;
use physics::{EYE_HEIGHT, look_direction};
use protocol::{EditRejection, MAX_PROJECTILES, ServerMessage};
use voxel_world::VoxelWorld;

impl Simulation {
    pub(super) fn fire_launcher(
        &mut self,
        world: &VoxelWorld,
        id: u64,
        request: u64,
        item: u32,
        yaw: f32,
        pitch: f32,
        power: u8,
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
        let launchers = self.packages.launcher_table();
        let launcher = launchers.get(item);
        let subticks_per_tick = launcher.map_or(0, |l| u64::from(l.shots_per_second));
        let origin = player.state.position + Vec3::Y * EYE_HEIGHT;
        let now = self.tick.saturating_mul(subticks_per_tick);
        let deadline = player.next_launch.get(&item).copied().unwrap_or(0);
        let rejection = if request <= player.highest_request {
            Some(EditRejection::OldRequest)
        } else if player.health.is_depleted() {
            Some(EditRejection::Dead)
        } else if launcher.is_none_or(|l| usize::from(power) >= l.powers().len()) {
            // No loaded package claims this item, or it authors no such preset.
            Some(EditRejection::PackageUnavailable)
        } else if now < deadline {
            Some(EditRejection::Cooldown)
        } else if !yaw.is_finite()
            || !pitch.is_finite()
            || pitch.abs() > std::f32::consts::FRAC_PI_2
        {
            Some(EditRejection::InvalidTarget)
        } else if world.block(origin.floor().as_ivec3()) != Some(0) {
            Some(EditRejection::Occupied)
        } else if self.projectiles.len() + self.detonations.len() >= MAX_PROJECTILES
            || self.next_projectile == u32::MAX
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
        let shot = launcher.expect("admitted item resolves").shot(power);
        let Ok(shot) = shot else {
            self.finish_edit(id, request, Err(EditRejection::PackageUnavailable));
            return;
        };
        let mut projectile = Projectile::from_shot(
            self.next_projectile,
            origin,
            look_direction(yaw, pitch),
            shot,
        );
        projectile.shooter = Some(id);
        self.projectiles.push(projectile);
        self.next_projectile += 1;
        self.projectile_revision += 1;
        self.metrics.launches += 1;
        let deadline = self
            .players
            .get_mut(&id)
            .unwrap()
            .next_launch
            .entry(item)
            .or_insert(0);
        // Carry sub-tick rounding forward for the package's authored firing rate.
        // After idle time, start a new cadence rather than banking burst shots.
        if now.saturating_sub(*deadline) >= subticks_per_tick {
            *deadline = now;
        }
        *deadline = deadline.saturating_add(60);
        self.finish_edit(id, request, Ok(None));
    }

    pub(super) fn advance_launchers(&mut self, world: &mut VoxelWorld) {
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
        let projectiles = std::mem::take(&mut self.projectiles);
        if !projectiles.is_empty() {
            self.projectile_revision += 1;
        }
        for mut projectile in projectiles {
            match projectile.tick(world, &bodies, ready) {
                Flight::Flying => self.projectiles.push(projectile),
                Flight::Impact(position) => self.detonations.push_back((
                    projectile.snapshot.id,
                    position,
                    projectile.shot.blast,
                    projectile.shooter.map(crate::SimEntity::Player),
                )),
                Flight::Expired => {}
            }
        }
        self.detonate_ready(world);
        if self.tick.is_multiple_of(3) {
            let recipients: Vec<_> = self
                .players
                .iter()
                .filter(|(_, player)| {
                    player.projectile_revision != Some(self.projectile_revision)
                })
                .map(|(&id, _)| id)
                .collect();
            let message = ServerMessage::Projectiles {
                tick: self.projectile_revision,
                projectiles: self
                    .projectiles
                    .iter()
                    .map(|projectile| projectile.snapshot)
                    .collect(),
            };
            for id in recipients {
                if self.send(id, &message) {
                    self.players.get_mut(&id).unwrap().projectile_revision =
                        Some(self.projectile_revision);
                }
            }
        }
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
    fn launcher_admits_authored_rate_without_idle_or_replay_bursts() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        sim.players.insert(1, Player::new());
        let mut request = 0;
        for tick in 0..600 {
            sim.tick = tick;
            // Retire projectiles here to isolate admission from the active cap.
            sim.projectiles.clear();
            request += 1;
            sim.fire_launcher(&world, 1, request, 6, 0.0, 0.0, 1);
            let shots = sim.metrics.launches;
            sim.fire_launcher(&world, 1, request, 6, 0.0, 0.0, 1);
            assert_eq!(sim.metrics.launches, shots);
            if tick % 60 == 59 {
                assert_eq!(shots, (tick / 60 + 1) * 25);
            }
        }
        sim.tick = 1200;
        sim.projectiles.clear();
        request += 1;
        sim.fire_launcher(&world, 1, request, 6, 0.0, 0.0, 1);
        assert_eq!(sim.metrics.launches, 251);
        for tick in 1200..1203 {
            sim.tick = tick;
            request += 1;
            sim.fire_launcher(&world, 1, request, 6, 0.0, 0.0, 1);
            assert_eq!(sim.metrics.launches, 251);
        }
        sim.tick = 1203;
        sim.fire_launcher(&world, 1, request + 1, 6, 0.0, 0.0, 1);
        assert_eq!(sim.metrics.launches, 252);
    }

    #[test]
    fn unknown_items_and_presets_never_spawn_projectiles() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut world = VoxelWorld::default();
        world.insert(IVec3::ZERO, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        sim.players.insert(1, Player::new());
        for (request, item, power) in [(1u64, 42u32, 0u8), (2, 6, 4), (3, 6, u8::MAX)] {
            sim.fire_launcher(&world, 1, request, item, 0.0, 0.0, power);
            assert_eq!(
                sim.players[&1].results.back().unwrap().1,
                Err(EditRejection::PackageUnavailable)
            );
        }
        assert!(sim.projectiles.is_empty());
        sim.fire_launcher(&world, 1, 4, 6, 0.0, 0.0, 0);
        assert_eq!(sim.projectiles.len(), 1);
    }

    #[test]
    fn each_shot_keeps_its_preset_through_impact_and_queued_detonation() {
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
        let presets = crate::packages::test_package().powers().len() as u8;
        for index in 0..presets {
            sim.tick = u64::from(index) * 10;
            sim.fire_launcher(&world, 1, u64::from(index) + 1, 6, yaw, 0.0, index);
        }
        assert_eq!(
            sim.projectiles
                .iter()
                .map(|projectile| projectile.shot.power)
                .collect::<Vec<_>>(),
            (0..presets).collect::<Vec<_>>()
        );
        // A replay with a changed preset must not alter an existing projectile.
        sim.fire_launcher(&world, 1, 1, 6, yaw, 0.0, 3);
        assert_eq!(sim.projectiles.len(), presets as usize);
        assert_eq!(sim.projectiles[0].shot.power, 0);
        // Occupy this tick's two detonation slots, retaining the new impacts in the queue.
        for id in [100, 101] {
            sim.detonations
                .push_back((id, Vec3::splat(24.0), crate::packages::test_blast(0), None));
        }
        sim.advance_launchers(&mut world);
        assert!(sim.projectiles.is_empty());
        assert_eq!(sim.metrics.explosions, 2);
        assert_eq!(
            sim.detonations
                .iter()
                .map(|(_, _, blast, _)| blast.radius)
                .collect::<Vec<_>>(),
            (0..presets)
                .map(|power| crate::packages::test_blast(power).radius)
                .collect::<Vec<_>>()
        );
        sim.advance_launchers(&mut world);
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
        sim.fire_launcher(&world, 1, 1, 6, 0.0, -1.2, 3);
        for _ in 0..10 {
            sim.advance_launchers(&mut world);
        }
        assert_eq!(sim.metrics.explosions, 1);
        let health = sim.player_health(1).unwrap();
        assert!(health.current() > 0 && health.current() < health.maximum());
        let launched = sim.players[&1].state;
        assert!(launched.velocity.y > 2.0, "{launched:?}");
        assert!(launched.external_velocity.y > 0.0);
        assert!(!launched.grounded);
        sim.advance_launchers(&mut world);
        assert_eq!(sim.players[&1].state, launched);
        assert_eq!(sim.player_health(1), Some(health));
        let mut after = launched;
        controller::step_player(&world, &mut after, &Default::default(), physics::FIXED_DT);
        assert!(after.position.y > launched.position.y);
        assert!(after.position.z > launched.position.z);
    }
}
