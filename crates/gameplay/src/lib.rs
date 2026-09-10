//! Shared gameplay components, independent of movement and collision physics.

use bevy_ecs::prelude::Component;
use serde::{Deserialize, Serialize};

pub const PLAYER_MAX_HEALTH: u16 = 100;

/// Bounded, whole-point health. The server owns mutations; clients display copies.
/// Depletion is a state, leaving death and respawn policy to the game.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "HealthValues")]
pub struct Health {
    current: u16,
    maximum: u16,
}

// Validate the same invariants on the wire as in the constructor.
#[derive(Deserialize)]
struct HealthValues {
    current: u16,
    maximum: u16,
}

impl TryFrom<HealthValues> for Health {
    type Error = &'static str;

    fn try_from(value: HealthValues) -> Result<Self, Self::Error> {
        if value.maximum == 0 || value.current > value.maximum {
            return Err("health requires a positive maximum and current <= maximum");
        }
        Ok(Self {
            current: value.current,
            maximum: value.maximum,
        })
    }
}

impl Default for Health {
    fn default() -> Self {
        Self {
            current: PLAYER_MAX_HEALTH,
            maximum: PLAYER_MAX_HEALTH,
        }
    }
}

impl Health {
    /// Create full health with a positive maximum; zero is invalid.
    pub fn new(maximum: u16) -> Option<Self> {
        (maximum > 0).then_some(Self {
            current: maximum,
            maximum,
        })
    }

    pub fn current(self) -> u16 {
        self.current
    }

    pub fn maximum(self) -> u16 {
        self.maximum
    }

    pub fn is_depleted(self) -> bool {
        self.current == 0
    }

    /// Remove health, returning the actual points lost, capped at current health.
    pub fn damage(&mut self, amount: u16) -> u16 {
        let applied = amount.min(self.current);
        self.current -= applied;
        applied
    }

    /// Add health, including from zero, returning the actual points restored.
    pub fn heal(&mut self, amount: u16) -> u16 {
        let applied = amount.min(self.maximum - self.current);
        self.current += applied;
        applied
    }

    /// Restore full health and return the points restored.
    pub fn restore(&mut self) -> u16 {
        self.heal(self.maximum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_starts_full_with_a_positive_maximum() {
        assert_eq!(Health::new(0), None);
        let health = Health::new(250).unwrap();
        assert_eq!(health.current(), 250);
        assert_eq!(health.maximum(), 250);
        assert!(!health.is_depleted());
        assert_eq!(Health::default().current(), PLAYER_MAX_HEALTH);
    }

    #[test]
    fn damage_reports_actual_loss_and_saturates_at_zero() {
        let mut health = Health::default();
        assert_eq!(health.damage(0), 0);
        assert_eq!(health.damage(30), 30);
        assert_eq!(health.current(), 70);
        assert_eq!(health.damage(u16::MAX), 70);
        assert!(health.is_depleted());
        assert_eq!(health.damage(1), 0);
        assert_eq!(health.maximum(), PLAYER_MAX_HEALTH);
    }

    #[test]
    fn healing_and_restore_are_bounded_even_at_integer_limits() {
        let mut health = Health::new(u16::MAX).unwrap();
        assert_eq!(health.heal(u16::MAX), 0);
        health.damage(u16::MAX);
        assert_eq!(health.heal(0), 0);
        assert_eq!(health.heal(10), 10);
        assert!(!health.is_depleted());
        assert_eq!(health.heal(u16::MAX), u16::MAX - 10);
        health.damage(123);
        assert_eq!(health.restore(), 123);
        assert_eq!(health.restore(), 0);
    }
}
