//! Melee resolution shared by the authoritative server and future policies.
//!
//! `resolve_swing` is pure: it reads a world snapshot and target list, returns
//! the single closest unobstructed hit, and mutates nothing. The host owns
//! cooldowns, health mutation, knockback application, and replication.
use glam::Vec3;
use physics::{CollisionShape, raycast_body};
use voxel_world::VoxelWorld;

/// Tunable melee weapon parameters. Values are validated at the boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeleeSpec {
    /// Maximum eye-to-target distance in metres.
    pub range: f32,
    /// Whole health points removed per hit.
    pub damage: u16,
    /// Fixed ticks between accepted swings.
    pub cooldown_ticks: u32,
    /// Impulse in kg·m/s applied along the swing direction.
    pub knockback: f32,
}

impl MeleeSpec {
    /// Reject non-finite or out-of-range tuning so hosts fail fast.
    pub fn bounded(self) -> Option<Self> {
        (self.range.is_finite()
            && (0.1..=16.0).contains(&self.range)
            && self.damage > 0
            && self.cooldown_ticks <= 600
            && self.knockback.is_finite()
            && (0.0..=10_000.0).contains(&self.knockback))
        .then_some(self)
    }
}

/// Default hands: a short, light swing suitable for the training dummy.
pub const MELEE_HANDS: MeleeSpec = MeleeSpec {
    range: 3.0,
    damage: 10,
    cooldown_ticks: 30,
    knockback: 320.0,
};

/// One resolved hit. `target` is the host's opaque target id; `distance` is the
/// ray entry distance; `impulse` is the knockback to apply to the victim.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SwingHit {
    pub target: u64,
    pub distance: f32,
    pub damage: u16,
    pub impulse: Vec3,
}

/// A swing target: opaque id, feet-anchored position, and collider shape.
#[derive(Clone, Copy, Debug)]
pub struct SwingTarget {
    pub id: u64,
    pub position: Vec3,
    pub shape: CollisionShape,
}

/// Resolve one melee swing against terrain occlusion and candidate targets.
/// Returns the closest living target the ray reaches within `spec.range`, or
/// `None` when terrain blocks first or nothing is hit. The ray starts at the
/// attacker's eye; `direction` need not be normalized.
pub fn resolve_swing(
    world: &VoxelWorld,
    origin: Vec3,
    direction: Vec3,
    spec: &MeleeSpec,
    targets: &[SwingTarget],
) -> Option<SwingHit> {
    let spec = spec.bounded()?;
    let direction = direction.try_normalize()?;
    if !origin.is_finite() {
        return None;
    }
    // Terrain closer than the target blocks the swing.
    let wall = world
        .raycast(origin, direction, spec.range)
        .map(|hit| hit.distance)
        .unwrap_or(spec.range);
    let mut best: Option<SwingHit> = None;
    for target in targets {
        let Some(distance) = raycast_body(origin, direction, target.position, target.shape)
        else {
            continue;
        };
        if distance > spec.range || distance >= wall {
            continue;
        }
        let impulse = direction * spec.knockback + Vec3::Y * spec.knockback * 0.25;
        let hit = SwingHit {
            target: target.id,
            distance,
            damage: spec.damage,
            impulse,
        };
        if best.is_none_or(|b| distance < b.distance) {
            best = Some(hit);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::IVec3;
    use voxel_world::Chunk;

    fn flat_world() -> VoxelWorld {
        let mut world = VoxelWorld::default();
        world.insert(
            IVec3::ZERO,
            Chunk::from_runs(0, &[(32768, 0)]).unwrap(),
        );
        world
    }

    fn target(id: u64, position: Vec3) -> SwingTarget {
        SwingTarget {
            id,
            position,
            shape: CollisionShape::default(),
        }
    }

    #[test]
    fn swing_hits_closest_target_and_reports_knockback() {
        let world = flat_world();
        let origin = Vec3::new(0.5, 1.6, 0.5);
        let targets = [
            target(7, Vec3::new(0.5, 0.0, -4.0)),
            target(3, Vec3::new(0.5, 0.0, -2.0)),
        ];
        let hit = resolve_swing(&world, origin, Vec3::NEG_Z, &MELEE_HANDS, &targets).unwrap();
        assert_eq!(hit.target, 3);
        assert_eq!(hit.damage, MELEE_HANDS.damage);
        assert!(hit.impulse.z < 0.0 && hit.impulse.y > 0.0);
    }

    #[test]
    fn swing_respects_range_terrain_and_dead_ends() {
        let mut world = flat_world();
        let origin = Vec3::new(0.5, 1.6, 0.5);
        // Beyond range.
        assert!(
            resolve_swing(
                &world,
                origin,
                Vec3::NEG_Z,
                &MELEE_HANDS,
                &[target(1, Vec3::new(0.5, 0.0, -10.0))],
            )
            .is_none()
        );
        // A wall between eye and target blocks the swing.
        world.insert(
            IVec3::new(0, 0, -1),
            Chunk::from_runs(0, &[(32768, 0)]).unwrap(),
        );
        world.set_block(IVec3::new(0, 1, -2), voxel_world::STONE).unwrap();
        assert!(
            resolve_swing(
                &world,
                origin,
                Vec3::NEG_Z,
                &MELEE_HANDS,
                &[target(1, Vec3::new(0.5, 0.0, -4.0))],
            )
            .is_none()
        );
        // No targets, no hit.
        assert!(resolve_swing(&world, origin, Vec3::NEG_Z, &MELEE_HANDS, &[]).is_none());
    }
}
