//! Authoritative blast transactions: queued impacts drain into damage,
//! impulses, a smooth terrain crater, and debris launch per bounded tick slice.
use super::{
    Simulation,
    explosion::{self, Target},
};
use glam::{IVec3, Vec3};
use physics::{PLAYER_MASS, apply_player_impulse};
use protocol::ServerMessage;
use voxel_world::{CHUNK_SIZE, VoxelWorld};

impl Simulation {
    pub(super) fn detonate_ready(&mut self, world: &mut VoxelWorld) {
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
                .map(|(&id, player)| (id, player.state))
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
    use protocol::EditRejection;
    use voxel_world::Chunk;

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
            sim.detonations
                .push_back((id, center, crate::packages::test_blast(1), None));
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
            1,
        )
        .into_iter()
        .find(|load| load.target == Target::Player(1))
        .unwrap()
        .player_damage;
        sim.detonations
            .push_back((1, center, crate::packages::test_blast(1), None));
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
        sim.fire_launcher(&world, 1, 1, 6, 0.0, 0.0, 1);
        assert_eq!(
            sim.players[&1].results.back().unwrap().1,
            Err(EditRejection::Dead)
        );
        assert!(sim.projectiles.is_empty());
        sim.detonations.push_back((
            1,
            Vec3::splat(10.0),
            crate::packages::test_blast(1),
            None,
        ));
        sim.detonate_ready(&mut world);
        assert_eq!(sim.players[&1].state.velocity, Vec3::ZERO);
        assert_eq!(sim.players[&1].state.external_velocity, glam::Vec2::ZERO);
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
        player.state.position = Vec3::new(10.5, 20.0, 10.5);
        player
            .known
            .insert(IVec3::ZERO, world.chunks[&IVec3::ZERO].revision);
        sim.players.insert(1, player);
        sim.detonations.push_back((
            1,
            Vec3::new(10.5, 10.0, 10.5),
            crate::packages::test_blast(1),
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
