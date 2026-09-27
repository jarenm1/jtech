//! Server-owned health access for gameplay systems and future physics impacts.

use super::{Simulation, terrain_stream};
use controller::PlayerInput;
use gameplay::Health;
use glam::Vec3;
use voxel_world::VoxelWorld;

impl Simulation {
    pub fn player_health(&self, id: u64) -> Option<Health> {
        self.players.get(&id).map(|player| player.health)
    }

    /// Apply server-authored damage. Returns points lost, or None for an absent player.
    /// Call once per accepted damage event, not once per replicated physics snapshot.
    /// The transition to zero is the one death event: movement input, queued inputs,
    /// momentum, body push, and queued strikes are cleared for that player.
    pub fn damage_player(&mut self, id: u64, amount: u16) -> Option<u16> {
        let player = self.players.get_mut(&id)?;
        let was_depleted = player.health.is_depleted();
        let lost = player.health.damage(amount);
        if !was_depleted && player.health.is_depleted() {
            player.input = PlayerInput {
                movement: [0.0; 2],
                jump: false,
                descend: false,
                attack: false,
                ..player.input
            };
            if let Some((&sequence, _)) = player.pending.last_key_value() {
                player.last_input = sequence;
            }
            player.respawn_requested = false;
            player.pending.clear();
            player.state.velocity = Vec3::ZERO;
            player.state.external_velocity = glam::Vec2::ZERO;
            player.body_push_velocity = Vec3::ZERO;
            // Full-loot death: the carried inventory scatters as drops.
            let position = player.state.position + Vec3::Y * 0.5;
            let spilled = player.inventory.entries().to_vec();
            player.inventory = gameplay::Inventory::new();
            player.inventory_dirty = true;
            self.cancel_player_strikes(id);
            for (index, (item, count)) in spilled.iter().enumerate() {
                // Fan stacks around the body so they read as a spill, not a point.
                let angle = index as f32 * 2.4;
                let offset = Vec3::new(angle.cos() * 0.4, 0.0, angle.sin() * 0.4);
                self.spawn_drop(position + offset, *item, *count);
            }
        }
        Some(lost)
    }

    /// Restore up to `amount` health for a living player. Returns points restored.
    /// A depleted player stays dead until an explicit respawn; this never revives
    /// a player in place.
    pub fn heal_player(&mut self, id: u64, amount: u16) -> Option<u16> {
        let player = self.players.get_mut(&id)?;
        if player.health.is_depleted() {
            return Some(0);
        }
        Some(player.health.heal(amount))
    }

    /// Fill a living player's health to its maximum. Returns points restored.
    /// A depleted player stays dead until an explicit respawn.
    pub fn restore_player_health(&mut self, id: u64) -> Option<u16> {
        let player = self.players.get_mut(&id)?;
        if player.health.is_depleted() {
            return Some(0);
        }
        Some(player.health.restore())
    }

    fn cancel_player_strikes(&mut self, id: u64) {
        let requests: Vec<_> = self
            .strikes
            .iter()
            .filter(|strike| strike.id == id)
            .map(|strike| strike.request)
            .collect();
        self.strikes.retain(|strike| strike.id != id);
        for request in requests {
            self.finish_edit(id, request, Err(protocol::EditRejection::Dead));
        }
    }

    pub(super) fn request_respawn(&mut self, id: u64, life: u64) {
        if let Some(player) = self.players.get_mut(&id)
            && player.health.is_depleted()
            && player.life == life
        {
            player.respawn_requested = true;
        }
    }

    pub(super) fn finish_respawns(&mut self, world: &VoxelWorld) {
        let requests: Vec<_> = self
            .players
            .iter()
            .filter(|(_, player)| player.respawn_requested)
            .map(|(&id, player)| (id, player.life))
            .collect();
        for (id, life) in requests {
            self.respawn_player(world, id, life);
        }
    }

    /// Respawn a dead player into loaded, supported, empty space near their
    /// bound bedroll — or the pinned world spawn when unbound — including
    /// clearance from loose colliders. Only a depleted player holding the
    /// current `life` token can spawn; success increments the token so replayed
    /// requests become stale. When no safe space is loaded yet the player stays
    /// dead and may retry with the same token. Returns whether the player is
    /// alive again.
    pub fn respawn_player(&mut self, world: &VoxelWorld, id: u64, life: u64) -> bool {
        let Some(player) = self.players.get(&id) else {
            return false;
        };
        if !player.health.is_depleted() || player.life != life {
            return false;
        }
        let Some(next_life) = life.checked_add(1) else {
            return false;
        };
        // Wait for the observation that owns the loose-body positions used for
        // clearance. A queued request is retried before the next GPU submission.
        if self
            .physics
            .as_ref()
            .is_some_and(|physics| physics.is_busy())
        {
            return false;
        }
        let bodies = self
            .physics
            .as_ref()
            .map(|physics| physics.dynamic_colliders())
            .unwrap_or_default();
        let anchor = self.spawn;
        let Some(position) = terrain_stream::available_spawn(world, anchor, &bodies) else {
            self.spawn_chunks
                .extend(terrain_stream::spawn_search_chunks(anchor));
            return false;
        };
        self.cancel_player_strikes(id);
        let loadout: Vec<(u32, u32)> = self.packages.melee_table().spawn_items().to_vec();
        let player = self.players.get_mut(&id).expect("respawn checked player");
        player.state.position = position;
        player.state.velocity = Vec3::ZERO;
        player.state.external_velocity = glam::Vec2::ZERO;
        player.state.grounded = false;
        player.state.noclip = false;
        player.input = PlayerInput {
            movement: [0.0; 2],
            jump: false,
            descend: false,
            noclip: false,
            ..player.input
        };
        player.pending.clear();
        player.body_push_velocity = Vec3::ZERO;
        player.health = Health::default();
        // Death scatters the old inventory; the package loadout replaces it.
        let melee = self.packages.melee_table();
        for (item, count) in loadout {
            if melee.kind(item) == protocol::ItemKind::Equipment {
                for _ in 0..count {
                    player.inventory.add_equipment(item);
                }
            } else {
                player.inventory.add(item, count);
            }
        }
        player.inventory_dirty = true;
        player.life = next_life;
        player.respawn_requested = false;
        player.physics_revision = None;
        player.interest_center = None;
        eprintln!("respawn id={id} life={} position={position:?}", player.life);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Player, ServerConfig, SimulationPlugin};
    use bevy_app::App;
    use protocol::{MAX_DATAGRAM, Snapshot, decode, encode};

    #[test]
    fn server_damage_healing_and_depletion_replicate_per_player() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        sim.players.insert(1, Player::new());
        sim.players.insert(2, Player::new());
        assert_eq!(sim.player_health(1), Some(Health::default()));
        assert_eq!(sim.damage_player(1, 25), Some(25));
        assert_eq!(sim.heal_player(1, 10), Some(10));
        assert_eq!(sim.player_health(1).unwrap().current(), 85);
        assert_eq!(sim.damage_player(1, u16::MAX), Some(85));
        assert_eq!(sim.damage_player(1, 10), Some(0));

        let snapshot = Snapshot {
            tick: sim.tick,
            you: sim.players[&1].snapshot(1),
            players: vec![sim.players[&2].snapshot(2)],
            actors: vec![],
        };
        let bytes = encode(&snapshot, MAX_DATAGRAM).unwrap();
        let received: Snapshot = decode(&bytes, MAX_DATAGRAM).unwrap();
        assert!(received.you.health.is_depleted());
        assert_eq!(received.you.life, 0);
        assert_eq!(received.players[0].health, Health::default());
        // A depleted player does not revive in place; respawn is the only path.
        assert_eq!(sim.heal_player(1, 50), Some(0));
        assert_eq!(sim.restore_player_health(1), Some(0));
        assert!(sim.players[&1].health.is_depleted());
    }

    #[test]
    fn death_spills_inventory_as_drops_and_clears_the_corpse() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        let mut player = Player::new();
        player.inventory.add(3, 40);
        player.inventory.add(5, 7);
        player.state.position = Vec3::new(1.5, 4.0, 2.5);
        sim.players.insert(1, player);

        sim.damage_player(1, u16::MAX);

        assert!(sim.players[&1].inventory.is_empty());
        assert!(sim.players[&1].inventory_dirty);
        let dropped: Vec<(u32, u32)> = sim
            .drops
            .iter()
            .map(|drop| (drop.snapshot.item, drop.snapshot.count))
            .collect();
        assert_eq!(dropped, vec![(3, 40), (5, 7)]);
        // A second death on the empty corpse spills nothing.
        sim.damage_player(1, 10);
        assert_eq!(sim.drops.len(), 2);
    }
    #[test]
    fn absent_and_disconnected_players_cannot_receive_health_changes() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        assert_eq!(sim.player_health(42), None);
        assert_eq!(sim.damage_player(42, 20), None);
        assert_eq!(sim.heal_player(42, 20), None);
        assert_eq!(sim.restore_player_health(42), None);
        sim.players.insert(42, Player::new());
        sim.damage_player(42, 50);
        sim.drop_player(42);
        assert_eq!(sim.damage_player(42, 20), None);
        sim.players.insert(43, Player::new());
        assert_eq!(sim.player_health(43), Some(Health::default()));
    }

    #[test]
    fn death_clears_input_momentum_and_queued_strikes_once() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        let mut player = Player::new();
        player.input.movement = [1.0, 0.0];
        player.pending.insert(
            1,
            PlayerInput {
                sequence: 1,
                ..Default::default()
            },
        );
        player.state.velocity = Vec3::new(4.0, -2.0, 1.0);
        player.state.external_velocity = glam::Vec2::new(3.0, 0.0);
        player.body_push_velocity = Vec3::new(2.0, 0.0, 0.0);
        sim.players.insert(1, player);
        sim.strikes.push_back(super::super::QueuedStrike {
            id: 1,
            request: 9,
            target: glam::IVec3::ZERO,
            revision: 0,
            expires: 999,
        });
        assert_eq!(sim.damage_player(1, u16::MAX), Some(100));
        {
            let player = &sim.players[&1];
            assert!(player.health.is_depleted());
            assert!(player.pending.is_empty());
            assert_eq!(player.input.movement, [0.0; 2]);
            assert_eq!(player.state.velocity, Vec3::ZERO);
            assert_eq!(player.state.external_velocity, glam::Vec2::ZERO);
            assert_eq!(player.body_push_velocity, Vec3::ZERO);
        }
        assert!(sim.strikes.is_empty());
        assert_eq!(
            sim.players[&1].results.back(),
            Some(&(9, Err(protocol::EditRejection::Dead)))
        );
        assert_eq!(sim.players[&1].last_input, 1);
        // Damage while already dead does not repeat the transition or clear state.
        sim.players.get_mut(&1).unwrap().state.velocity = Vec3::new(1.0, 1.0, 1.0);
        assert_eq!(sim.damage_player(1, 10), Some(0));
        assert_eq!(sim.players[&1].state.velocity, Vec3::new(1.0, 1.0, 1.0));
    }

    #[test]
    fn dead_players_cannot_edit_or_strike() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut world = app.world_mut().remove_resource::<VoxelWorld>().unwrap();
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        sim.players.insert(1, Player::new());
        sim.damage_player(1, u16::MAX);
        let target = glam::IVec3::new(1, 1, 1);
        sim.edit(&mut world, 1, 1, target, 0, 0, false);
        sim.edit(&mut world, 1, 2, target, 0, 0, true);
        assert_eq!(sim.players[&1].results.len(), 2);
        assert!(
            sim.players[&1]
                .results
                .iter()
                .all(|(_, outcome)| *outcome == Err(protocol::EditRejection::Dead))
        );
        // A queued strike for a dead player is rejected instead of detaching.
        sim.strikes.push_back(super::super::QueuedStrike {
            id: 1,
            request: 3,
            target,
            revision: 0,
            expires: 999,
        });
        sim.drain_strikes(&mut world);
        assert_eq!(sim.players[&1].results.len(), 3);
        assert_eq!(
            sim.players[&1].results.back().unwrap().1,
            Err(protocol::EditRejection::Dead)
        );
    }

    #[test]
    fn respawn_requires_depletion_and_matching_life() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        sim.players.insert(1, Player::new());
        // A living player ignores respawn requests.
        assert!(!sim.respawn_player(&VoxelWorld::default(), 1, 0));
        assert_eq!(sim.players[&1].life, 0);
        sim.damage_player(1, u16::MAX);
        // Without loaded supported space the player stays dead for a later retry.
        assert!(!sim.respawn_player(&VoxelWorld::default(), 1, 1));
        assert!(sim.players[&1].health.is_depleted());
        assert_eq!(sim.players[&1].life, 0);
    }

    #[test]
    fn respawn_uses_loaded_support_and_increments_life_once() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let world = app.world_mut().remove_resource::<VoxelWorld>().unwrap();
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        let mut player = Player::new();
        player.state.noclip = true;
        player.state.external_velocity = glam::Vec2::new(5.0, 5.0);
        player.body_push_velocity = Vec3::new(1.0, 1.0, 1.0);
        player.last_input = 40;
        player.highest_request = 7;
        player.last_edit = 3;
        sim.players.insert(1, player);
        sim.damage_player(1, u16::MAX);
        assert!(sim.respawn_player(&world, 1, 0));
        assert_eq!(sim.players[&1].life, 1);
        assert_eq!(sim.player_health(1), Some(Health::default()));
        // The package loadout is re-granted after death scattered the old gear.
        for item in [7u32, 8, 9] {
            assert_eq!(sim.players[&1].inventory.count(item), 1);
        }
        let player = &sim.players[&1];
        assert!(!player.state.noclip);
        assert_eq!(player.state.velocity, Vec3::ZERO);
        assert_eq!(player.state.external_velocity, glam::Vec2::ZERO);
        assert_eq!(player.body_push_velocity, Vec3::ZERO);
        // Request dedup, cadence, and the monotonic input sequence survive respawn.
        assert_eq!(player.last_input, 40);
        assert_eq!(player.highest_request, 7);
        assert_eq!(player.last_edit, 3);
        // The landing cell is supported and clear of terrain.
        let feet = player.state.position.floor().as_ivec3();
        assert_ne!(world.block(feet - glam::IVec3::Y), Some(voxel_world::AIR));
        for cell in [feet, feet + glam::IVec3::Y, feet + glam::IVec3::Y * 2] {
            assert_eq!(world.block(cell), Some(voxel_world::AIR));
        }
        // A duplicate of the consumed token cannot spawn twice.
        assert!(!sim.respawn_player(&world, 1, 0));
        assert_eq!(sim.players[&1].life, 1);
        // A later death only accepts the current life token.
        sim.damage_player(1, u16::MAX);
        assert!(!sim.respawn_player(&world, 1, 0));
        assert!(sim.players[&1].health.is_depleted());
        assert!(sim.respawn_player(&world, 1, 1));
        assert_eq!(sim.players[&1].life, 2);
    }

    #[test]
    fn long_death_discards_but_acknowledges_input_and_accepts_new_life() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let world = app.world_mut().remove_resource::<VoxelWorld>().unwrap();
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        sim.players.insert(1, Player::new());
        sim.damage_player(1, 100);
        let state = sim.players[&1].state;
        for sequence in 1..=600 {
            sim.accept_inputs(
                1,
                protocol::InputPacket {
                    session: 0,
                    life: 0,
                    inputs: vec![PlayerInput {
                        sequence,
                        movement: [1.0, 0.0],
                        noclip: true,
                        ..Default::default()
                    }],
                },
            );
        }
        assert_eq!(sim.players[&1].last_input, 600);
        assert_eq!(sim.players[&1].state, state);
        assert!(sim.players[&1].pending.is_empty());
        assert!(sim.respawn_player(&world, 1, 0));
        sim.accept_inputs(
            1,
            protocol::InputPacket {
                session: 0,
                life: 1,
                inputs: vec![PlayerInput {
                    sequence: 601,
                    ..Default::default()
                }],
            },
        );
        assert!(sim.players[&1].pending.contains_key(&601));
    }

    #[test]
    fn queued_respawn_loads_fallback_volume_and_completes_once() {
        let mut app = App::new();
        app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
        let mut world = VoxelWorld::default();
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        sim.spawn = Vec3::new(0.5, 70.0, 0.5);
        sim.players.insert(1, Player::new());
        sim.damage_player(1, 100);
        sim.request_respawn(1, 0);
        sim.finish_respawns(&world);
        assert!(sim.players[&1].health.is_depleted());
        assert!(sim.players[&1].respawn_requested);
        let ground = glam::IVec3::new(0, 15, 0);
        let coord = voxel_world::chunk_coord(ground);
        assert!(sim.spawn_chunks.contains(&coord));
        // Model a completed fallback chunk after excavation emptied the warm area.
        world.insert(
            coord,
            voxel_world::Chunk::from_runs(0, &[(32768, 0)]).unwrap(),
        );
        world.set_block(ground, 3).unwrap();
        sim.finish_respawns(&world);
        assert_eq!(sim.players[&1].state.position, Vec3::new(0.5, 16.0, 0.5));
        assert_eq!(sim.players[&1].life, 1);
        assert!(!sim.players[&1].respawn_requested);
        sim.finish_respawns(&world);
        assert_eq!(sim.players[&1].life, 1);
    }

    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn respawn_waits_for_the_in_flight_body_observation() {
        let _guard = crate::physics_slice::GPU_TEST_LOCK.lock().unwrap();
        let mut app = App::new();
        app.add_plugins(
            SimulationPlugin::headless(ServerConfig {
                gpu_physics: true,
                ..Default::default()
            })
            .unwrap(),
        );
        let mut world = app.world_mut().remove_resource::<VoxelWorld>().unwrap();
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        sim.players.insert(1, Player::new());
        sim.damage_player(1, 100);
        sim.request_respawn(1, 0);
        let target = (sim.spawn + Vec3::Y * 5.0).floor().as_ivec3();
        let physics = sim.physics.as_mut().unwrap();
        physics.release(target, 3, 0.0, Vec3::ZERO);
        for _ in 0..3 {
            physics.step(&world);
        }
        assert!(physics.is_busy());
        sim.finish_respawns(&world);
        assert!(sim.players[&1].health.is_depleted());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while sim.physics.as_ref().unwrap().is_busy() {
            sim.observe_physics(&mut world);
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!sim.physics.as_ref().unwrap().failed());
        sim.finish_respawns(&world);
        assert_eq!(sim.players[&1].life, 1);
        assert_eq!(sim.player_health(1), Some(Health::default()));
        assert_eq!(sim.players[&1].state.velocity, Vec3::ZERO);
    }
}
