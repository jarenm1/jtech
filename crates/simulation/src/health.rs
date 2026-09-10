//! Server-owned health access for gameplay systems and future physics impacts.

use super::Simulation;
use gameplay::Health;

impl Simulation {
    pub fn player_health(&self, id: u64) -> Option<Health> {
        self.players.get(&id).map(|player| player.health)
    }

    /// Apply server-authored damage. Returns points lost, or None for an absent player.
    /// Call once per accepted damage event, not once per replicated physics snapshot.
    pub fn damage_player(&mut self, id: u64, amount: u16) -> Option<u16> {
        Some(self.players.get_mut(&id)?.health.damage(amount))
    }

    /// Restore up to `amount` health, including from depletion. Returns points restored.
    pub fn heal_player(&mut self, id: u64, amount: u16) -> Option<u16> {
        Some(self.players.get_mut(&id)?.health.heal(amount))
    }

    /// Fill health to its maximum. Position and movement are managed separately.
    pub fn restore_player_health(&mut self, id: u64) -> Option<u16> {
        Some(self.players.get_mut(&id)?.health.restore())
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
        };
        let bytes = encode(&snapshot, MAX_DATAGRAM).unwrap();
        let received: Snapshot = decode(&bytes, MAX_DATAGRAM).unwrap();
        assert!(received.you.health.is_depleted());
        assert_eq!(received.players[0].health, Health::default());
        assert_eq!(sim.restore_player_health(1), Some(100));
        assert_eq!(sim.restore_player_health(1), Some(0));
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
}
