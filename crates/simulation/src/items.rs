//! Authoritative dropped items. Destruction leaves a floating pickup that falls,
//! settles, and merges into a nearby player's inventory. Item identity is data
//! here so a Scheme registry can supply richer behavior later.
use super::Simulation;
use gameplay::Inventory;
use glam::Vec3;
use protocol::{DropSnapshot, MAX_DROPS, ServerMessage};
use voxel_world::{AIR, BEDROLL, VoxelWorld};

/// Fall acceleration for a dropped item.
const DROP_GRAVITY: f32 = 18.0;
/// Upward pop so a drop clears the cell it came from.
const DROP_POP: f32 = 1.6;
/// Lateral spread applied from the drop id, in metres per second.
const DROP_SPREAD: f32 = 0.5;
/// Half-height of a dropped item, used to rest it on a block face.
const DROP_RADIUS: f32 = 0.15;
/// Reach from a player's centre that collects a drop.
const PICKUP_RADIUS: f32 = 1.8;
/// A drop cannot be collected until it has been visible this long.
const PICKUP_DELAY_TICKS: u32 = 30;
/// Dropped items despawn after five minutes so the bounded set keeps churning.
const DROP_LIFETIME_TICKS: u32 = 18_000;

#[derive(Clone, Copy, Debug)]
pub(super) struct Drop {
    pub snapshot: DropSnapshot,
    velocity: Vec3,
    settled: bool,
    age: u32,
}

impl Drop {
    fn new(id: u32, position: Vec3, item: u8, count: u16) -> Self {
        // The golden angle spreads successive drops without shared RNG state.
        let angle = id as f32 * 2.399_963_2;
        let velocity = Vec3::new(angle.cos(), 0.0, angle.sin()) * DROP_SPREAD + Vec3::Y * DROP_POP;
        Self {
            snapshot: DropSnapshot {
                id,
                item,
                count,
                position,
            },
            velocity,
            settled: false,
            age: 0,
        }
    }

    /// Advance one fixed step. Returns whether the replicated snapshot changed.
    pub(super) fn step(&mut self, world: &VoxelWorld) -> bool {
        if self.settled {
            let support = (self.snapshot.position - Vec3::Y * (DROP_RADIUS + 0.01))
                .floor()
                .as_ivec3();
            if world.block(support) == Some(AIR) {
                // The supporting block was removed; resume falling.
                self.settled = false;
                return true;
            }
            return false;
        }
        self.velocity.y -= DROP_GRAVITY * physics::FIXED_DT;
        let candidate = self.snapshot.position + self.velocity * physics::FIXED_DT;
        let cell = candidate.floor().as_ivec3();
        match world.block(cell) {
            // Unknown terrain: hold until the chunk is resident.
            None => false,
            Some(AIR) => {
                self.snapshot.position = candidate;
                true
            }
            Some(_) => {
                self.snapshot.position =
                    Vec3::new(candidate.x, cell.y as f32 + 1.0 + DROP_RADIUS, candidate.z);
                self.velocity = Vec3::ZERO;
                self.settled = true;
                true
            }
        }
    }

    /// Merge as much of this stack as `inventory` accepts; returns the amount taken.
    pub(super) fn collect(&mut self, inventory: &mut Inventory) -> u16 {
        let taken = inventory.add(self.snapshot.item, self.snapshot.count);
        self.snapshot.count -= taken;
        taken
    }
}

impl Simulation {
    /// Add up to `amount` of `item` to a player's inventory. Returns the amount
    /// accepted before the stack ceiling; unknown players return `None`.
    pub fn grant_item(&mut self, id: u64, item: u8, amount: u16) -> Option<u16> {
        let player = self.players.get_mut(&id)?;
        let granted = player.inventory.add(item, amount);
        if granted > 0 {
            player.inventory_dirty = true;
        }
        Some(granted)
    }
}

impl Simulation {
    /// Leave a dropped stack at `position`. The authoritative destruction path
    /// calls this for grid and loose-block destruction alike.
    pub(super) fn spawn_drop(&mut self, position: Vec3, item: u8, count: u16) {
        if item == 0 || item > BEDROLL || count == 0 || !position.is_finite() {
            return;
        }
        if self.drops.len() >= MAX_DROPS {
            self.drops.pop_front();
        }
        let id = self.next_drop;
        self.next_drop = self.next_drop.wrapping_add(1).max(1);
        self.drops.push_back(Drop::new(id, position, item, count));
        self.drop_revision += 1;
    }

    /// Integrate, settle and expire dropped items against resident terrain.
    pub(super) fn advance_drops(&mut self, world: &VoxelWorld) {
        let mut changed = false;
        self.drops.retain_mut(|drop| {
            drop.age += 1;
            if drop.age > DROP_LIFETIME_TICKS {
                changed = true;
                return false;
            }
            changed |= drop.step(world);
            true
        });
        if changed {
            self.drop_revision += 1;
        }
    }

    /// Give living players the drops within reach, lowest player id first.
    pub(super) fn collect_drops(&mut self) {
        if self.drops.is_empty() || self.players.is_empty() {
            return;
        }
        let mut players: Vec<(u64, Vec3)> = self
            .players
            .iter()
            .filter(|(_, player)| !player.health.is_depleted())
            .map(|(&id, player)| {
                (
                    id,
                    player.state.position + Vec3::Y * (physics::PLAYER_HEIGHT * 0.5),
                )
            })
            .collect();
        players.sort_unstable_by_key(|(id, _)| *id);
        let mut changed = false;
        for (id, center) in players {
            let mut index = 0;
            while index < self.drops.len() {
                let drop = &mut self.drops[index];
                if drop.age < PICKUP_DELAY_TICKS
                    || drop.snapshot.position.distance(center) > PICKUP_RADIUS
                {
                    index += 1;
                    continue;
                }
                let inventory = &mut self.players.get_mut(&id).unwrap().inventory;
                if drop.collect(inventory) == 0 {
                    // The stack is full; leave the remainder for later or for others.
                    index += 1;
                    continue;
                }
                self.players.get_mut(&id).unwrap().inventory_dirty = true;
                changed = true;
                if drop.snapshot.count == 0 {
                    self.drops.remove(index);
                } else {
                    index += 1;
                }
            }
        }
        if changed {
            self.drop_revision += 1;
        }
    }

    /// Reliably resend the whole bounded set after any spawn, move or removal.
    pub(super) fn replicate_drops(&mut self) {
        if self.drop_revision == self.sent_drop_revision {
            return;
        }
        self.sent_drop_revision = self.drop_revision;
        let drops: Vec<_> = self.drops.iter().map(|drop| drop.snapshot).collect();
        let ids: Vec<_> = self.players.keys().copied().collect();
        for id in ids {
            self.send(
                id,
                &ServerMessage::Drops {
                    tick: self.tick,
                    drops: drops.clone(),
                },
            );
        }
    }

    /// Send owned-item counts for players whose inventory changed.
    pub(super) fn replicate_inventory(&mut self) {
        let dirty: Vec<_> = self
            .players
            .iter()
            .filter(|(_, player)| player.inventory_dirty)
            .map(|(&id, _)| id)
            .collect();
        for id in dirty {
            let inventory = self.players[&id].inventory;
            if self.send(id, &ServerMessage::Inventory { inventory }) {
                self.players.get_mut(&id).unwrap().inventory_dirty = false;
            }
        }
    }
}
