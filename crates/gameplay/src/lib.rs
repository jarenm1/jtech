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

/// Item kinds addressable by an inventory. Ids 1 through 5 mirror the placeable
/// block materials and 0 is empty; a later item registry can widen the mapping
/// without changing the container shape.
pub const INVENTORY_SLOTS: usize = 6;
/// Ceiling for one item kind's count.
pub const MAX_STACK: u16 = 999;

/// Bounded per-item counts gathered from the world. The server owns mutations;
/// clients display the replicated copy.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "InventoryCounts")]
pub struct Inventory {
    counts: [u16; INVENTORY_SLOTS],
}

// Validate the same invariants on the wire as in the mutators.
#[derive(Deserialize)]
struct InventoryCounts {
    counts: [u16; INVENTORY_SLOTS],
}

impl TryFrom<InventoryCounts> for Inventory {
    type Error = &'static str;

    fn try_from(value: InventoryCounts) -> Result<Self, Self::Error> {
        if value.counts.iter().any(|&count| count > MAX_STACK) {
            return Err("inventory counts exceed the stack ceiling");
        }
        Ok(Self {
            counts: value.counts,
        })
    }
}

impl Default for Inventory {
    fn default() -> Self {
        Self {
            counts: [0; INVENTORY_SLOTS],
        }
    }
}

impl Inventory {
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(item: u8) -> Option<usize> {
        (1..INVENTORY_SLOTS as u8)
            .contains(&item)
            .then_some(item as usize)
    }

    pub fn count(self, item: u8) -> u16 {
        Self::slot(item).map_or(0, |slot| self.counts[slot])
    }

    pub fn total(self) -> u32 {
        self.counts.iter().map(|&count| u32::from(count)).sum()
    }

    pub fn is_empty(self) -> bool {
        self.counts.iter().all(|&count| count == 0)
    }

    pub fn counts(&self) -> &[u16; INVENTORY_SLOTS] {
        &self.counts
    }

    /// Add up to `amount`, returning how many were accepted before the ceiling.
    pub fn add(&mut self, item: u8, amount: u16) -> u16 {
        let Some(slot) = Self::slot(item) else {
            return 0;
        };
        let accepted = amount.min(MAX_STACK - self.counts[slot]);
        self.counts[slot] += accepted;
        accepted
    }

    /// Remove up to `amount`, returning how many were taken.
    pub fn take(&mut self, item: u8, amount: u16) -> u16 {
        let Some(slot) = Self::slot(item) else {
            return 0;
        };
        let taken = amount.min(self.counts[slot]);
        self.counts[slot] -= taken;
        taken
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

    #[test]
    fn inventory_adds_takes_and_rejects_unknown_items() {
        // Item ids 1 through 5 are the placeable block materials.
        let mut inventory = Inventory::new();
        assert!(inventory.is_empty());
        assert_eq!(inventory.total(), 0);
        assert_eq!(inventory.add(0, 5), 0);
        assert_eq!(inventory.add(INVENTORY_SLOTS as u8, 5), 0);
        assert_eq!(inventory.add(3, 3), 3);
        assert_eq!(inventory.add(3, 2), 2);
        assert_eq!(inventory.count(3), 5);
        assert_eq!(inventory.count(2), 0);
        assert_eq!(inventory.total(), 5);
        assert_eq!(inventory.take(3, 2), 2);
        assert_eq!(inventory.take(3, 99), 3);
        assert!(inventory.is_empty());
        assert_eq!(inventory.take(3, 1), 0);
    }

    #[test]
    fn inventory_saturates_at_the_stack_ceiling() {
        let mut inventory = Inventory::new();
        assert_eq!(inventory.add(5, MAX_STACK - 1), MAX_STACK - 1);
        assert_eq!(inventory.add(5, u16::MAX), 1);
        assert_eq!(inventory.count(5), MAX_STACK);
        assert_eq!(inventory.add(5, 1), 0);
        assert_eq!(inventory.counts()[5], MAX_STACK);
    }
}
