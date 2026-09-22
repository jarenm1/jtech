//! Shared gameplay components and rules. Health and inventory stay free of
//! physics types; `combat` resolves melee swings against voxel terrain and
//! character shapes.

pub mod combat;

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

/// One owned stack: an item id and its count. Item 0 is never stored.
pub type ItemStack = (u8, u32);

/// Unbounded per-item stacks gathered from the world, in first-seen order.
/// The server owns mutations; clients display the replicated copy. Counts are
/// uncapped: `add` saturates at `u32::MAX` rather than rejecting.
#[derive(Component, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "InventoryEntries")]
pub struct Inventory {
    entries: Vec<ItemStack>,
}

// Reject malformed wire data: item 0 or zero counts. Duplicate ids are legal:
// equipment occupies one entry per instance so future per-item meta has a home.
#[derive(Deserialize)]
struct InventoryEntries {
    entries: Vec<ItemStack>,
}

impl TryFrom<InventoryEntries> for Inventory {
    type Error = &'static str;

    fn try_from(value: InventoryEntries) -> Result<Self, Self::Error> {
        for &(item, count) in &value.entries {
            if item == 0 || count == 0 {
                return Err("inventory entries must be nonzero stacks");
            }
        }
        Ok(Self {
            entries: value.entries,
        })
    }
}

impl Inventory {
    pub fn new() -> Self {
        Self::default()
    }

    fn position(&self, item: u8) -> Option<usize> {
        self.entries.iter().position(|&(id, _)| id == item)
    }

    /// Total owned count across every entry for `item`; equipment instances
    /// each contribute their own entry.
    pub fn count(&self, item: u8) -> u32 {
        self.entries
            .iter()
            .filter(|&&(id, _)| id == item)
            .map(|&(_, count)| count)
            .sum()
    }

    pub fn total(&self) -> u64 {
        self.entries.iter().map(|&(_, count)| u64::from(count)).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Owned stacks in first-seen order.
    pub fn entries(&self) -> &[ItemStack] {
        &self.entries
    }

    /// Add `amount`, appending a new stack for unseen items. Returns the
    /// amount accepted; only item 0 or a saturated count is refused.
    pub fn add(&mut self, item: u8, amount: u32) -> u32 {
        if item == 0 || amount == 0 {
            return 0;
        }
        match self.position(item) {
            Some(index) => {
                let accepted = amount.min(u32::MAX - self.entries[index].1);
                self.entries[index].1 += accepted;
                accepted
            }
            None => {
                self.entries.push((item, amount));
                amount
            }
        }
    }

    /// Add one equipment instance as its own entry, even when the item is
    /// already owned. Returns 1, or 0 for item 0.
    pub fn add_equipment(&mut self, item: u8) -> u32 {
        if item == 0 {
            return 0;
        }
        self.entries.push((item, 1));
        1
    }

    /// Remove up to `amount` across every entry for `item`, returning how many
    /// were taken. Empty entries are dropped from the list.
    pub fn take(&mut self, item: u8, amount: u32) -> u32 {
        let mut remaining = amount;
        let mut index = 0;
        while remaining > 0 && index < self.entries.len() {
            if self.entries[index].0 != item {
                index += 1;
                continue;
            }
            let taken = remaining.min(self.entries[index].1);
            self.entries[index].1 -= taken;
            remaining -= taken;
            if self.entries[index].1 == 0 {
                self.entries.remove(index);
            } else {
                index += 1;
            }
        }
        amount - remaining
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
    fn inventory_adds_takes_and_appends_new_items() {
        let mut inventory = Inventory::new();
        assert!(inventory.is_empty());
        assert_eq!(inventory.total(), 0);
        assert_eq!(inventory.add(0, 5), 0);
        assert_eq!(inventory.add(3, 3), 3);
        assert_eq!(inventory.add(3, 2), 2);
        assert_eq!(inventory.add(7, 4), 4);
        assert_eq!(inventory.count(3), 5);
        assert_eq!(inventory.count(7), 4);
        assert_eq!(inventory.count(2), 0);
        assert_eq!(inventory.total(), 9);
        assert_eq!(inventory.entries(), &[(3, 5), (7, 4)]);
        assert_eq!(inventory.take(3, 2), 2);
        assert_eq!(inventory.take(3, 99), 3);
        assert_eq!(inventory.entries(), &[(7, 4)]);
        assert_eq!(inventory.take(7, 4), 4);
        assert!(inventory.is_empty());
        assert_eq!(inventory.take(3, 1), 0);
    }

    #[test]
    fn equipment_instances_take_separate_entries() {
        let mut inventory = Inventory::new();
        assert_eq!(inventory.add_equipment(7), 1);
        assert_eq!(inventory.add_equipment(7), 1);
        assert_eq!(inventory.add_equipment(0), 0);
        assert_eq!(inventory.entries(), &[(7, 1), (7, 1)]);
        assert_eq!(inventory.count(7), 2);
        assert_eq!(inventory.take(7, 1), 1);
        assert_eq!(inventory.entries(), &[(7, 1)]);
        assert_eq!(inventory.take(7, 5), 1);
        assert!(inventory.is_empty());
    }

    #[test]
    fn inventory_counts_saturate_instead_of_rejecting() {
        let mut inventory = Inventory::new();
        assert_eq!(inventory.add(5, u32::MAX - 1), u32::MAX - 1);
        assert_eq!(inventory.add(5, 10), 1);
        assert_eq!(inventory.count(5), u32::MAX);
    }
}
