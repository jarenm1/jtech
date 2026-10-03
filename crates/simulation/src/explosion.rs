//! Bounded blast allocation against one pre-explosion world observation.
//! Materials share a finite charge; player knockback has a separate authored budget.
use game_packages::BlastSpec;
use glam::{IVec3, Vec3};
use gpu_physics::{TerrainContact, material};
use physics::{PLAYER_HEIGHT, PLAYER_MASS, PlayerState};
#[cfg(test)]
use protocol::BowPower;
use protocol::PhysicsBodySnapshot;
use voxel_world::{CHUNK_SIZE, MIN_CHUNK_Y, VoxelWorld};

// A full-health player survives one blast at any preset. Larger radii extend
// the dangerous area; repeated close shots are lethal without sacrificing jumps.
const MAX_PLAYER_BLAST_DAMAGE: f32 = 60.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Target {
    Grid(IVec3),
    Body(u32),
    Player(u64),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct BlastLoad {
    pub target: Target,
    pub contact: TerrainContact,
    pub direction: Vec3,
    pub kinetic_energy: f32,
    /// Whole health points from the same frozen exposure samples as knockback.
    pub player_damage: u16,
}

/// Positive outward impulse whose change in kinetic energy is the reserved budget.
/// Include existing velocity and queued impulses, not just the rest-mass formula.
pub(super) fn kinetic_impulse(mass: f32, velocity: Vec3, direction: Vec3, energy: f32) -> Vec3 {
    if energy <= 0.0 {
        return Vec3::ZERO;
    }
    let speed = velocity.dot(direction);
    let root = (speed * speed + 2.0 * energy / mass).sqrt();
    // Rationalized root avoids cancellation when already moving outward quickly.
    let impulse = if speed >= 0.0 {
        2.0 * energy / (root + speed)
    } else {
        mass * (root - speed)
    };
    direction * impulse
}

#[cfg(test)]
pub(super) fn plan(
    world: &VoxelWorld,
    bodies: &[PhysicsBodySnapshot],
    players: &[(u64, PlayerState)],
    center: Vec3,
    power: BowPower,
) -> Vec<BlastLoad> {
    plan_blast(
        world,
        bodies,
        players,
        center,
        crate::packages::test_blast(power),
    )
}
/// Sample facing cube-face centers, with 1m² effective area and radial falloff.
/// Freeze the receivers before changing terrain, so removing the front of a wall
/// cannot expose its back to the same explosion.
pub(super) fn plan_blast(
    world: &VoxelWorld,
    bodies: &[PhysicsBodySnapshot],
    players: &[(u64, PlayerState)],
    center: Vec3,
    blast: BlastSpec,
) -> Vec<BlastLoad> {
    if !center.is_finite() {
        return Vec::new();
    }
    let radius = blast.radius;
    let min = (center - Vec3::splat(radius)).floor().as_ivec3();
    let max = (center + Vec3::splat(radius)).floor().as_ivec3();
    // Bodies outside the blast sphere cannot be receivers, and a unit-cube body
    // can only occlude a ray ending within `radius` if its centre is within
    // `radius + 1`. Filter once so the per-face occlusion scans below touch only
    // nearby bodies instead of every body in the world (was O(candidates·bodies)).
    let reach_limit = radius + 1.0;
    let nearby: Vec<&PhysicsBodySnapshot> = bodies
        .iter()
        .filter(|body| (body.position - center).length_squared() <= reach_limit * reach_limit)
        .collect();
    let span = (max - min + IVec3::ONE).max(IVec3::ZERO);
    let cells = (span.x as usize) * (span.y as usize) * (span.z as usize);
    let mut candidates = Vec::with_capacity(cells + nearby.len());
    for y in min.y..=max.y {
        if y <= MIN_CHUNK_Y * CHUNK_SIZE {
            continue;
        }
        for z in min.z..=max.z {
            for x in min.x..=max.x {
                let cell = IVec3::new(x, y, z);
                if let Some(material @ 1..=5) = world.block(cell) {
                    candidates.push((
                        Target::Grid(cell),
                        cell.as_vec3() + Vec3::splat(0.5),
                        material as u32,
                    ));
                }
            }
        }
    }
    for body in &nearby {
        candidates.push((Target::Body(body.id), body.position, body.material as u32));
    }
    let mut visible = Vec::with_capacity(candidates.len());
    for (target, position, mat) in candidates {
        let offset = position - center;
        let distance = offset.length();
        if distance >= radius {
            continue;
        }
        let direction = offset.try_normalize().unwrap_or(Vec3::Y);
        // Trace to exposed faces, not the cube center: at a wall impact a
        // center ray would enter the struck voxel before reaching its neighbors.
        // A moving body can overlap the arrow between completed observations.
        let contains_blast = offset.abs().cmple(Vec3::splat(0.5)).all();
        let mut surface_normal = Vec3::ZERO;
        for axis in 0..3 {
            if offset[axis].abs() <= 0.5 {
                continue;
            }
            let mut normal = Vec3::ZERO;
            normal[axis] = -offset[axis].signum();
            let face = position + normal * 0.5;
            let ray = face - center;
            let reach = ray.length();
            let aim = ray / reach;
            let terrain_hit = world.raycast(center, aim, reach + 0.0001);
            let terrain_clear = match target {
                Target::Grid(cell) => terrain_hit.is_some_and(|hit| hit.block == cell),
                Target::Body(_) | Target::Player(_) => terrain_hit.is_none(),
            };
            if terrain_clear
                && !nearby.iter().any(|body| {
                    target != Target::Body(body.id)
                        && super::bow::ray_cube(center, aim, body.position, reach).is_some()
                })
            {
                surface_normal += normal * offset[axis].abs();
            }
        }
        if !contains_blast && surface_normal == Vec3::ZERO {
            continue;
        }
        // Authored crater ejection: bias released terrain toward exposed air.
        // Pure radial pressure drives floor/wall blocks back into their support.
        // Change direction only; the kinetic budget still determines the impulse.
        let direction = if matches!(target, Target::Grid(_)) && !contains_blast {
            let normal = surface_normal.normalize();
            (direction + normal * 2.0).normalize()
        } else {
            direction
        };
        let falloff = (1.0 - distance / radius).powi(2);
        let weight = falloff / (distance * distance + 0.25);
        visible.push((target, mat, direction, weight));
    }
    // Sample the actor above the feet so a supporting floor does not occlude it.
    // Do not dilute character knockback among the many cheap voxel receivers.
    for &(id, state) in players {
        if state.noclip || !state.position.is_finite() {
            continue;
        }
        let offset = state.position + Vec3::Y * (PLAYER_HEIGHT * 0.5) - center;
        let distance = offset.length();
        if distance >= radius {
            continue;
        }
        let exposed = [0.25, 0.5, 0.75]
            .into_iter()
            .filter(|height| {
                let sample = state.position + Vec3::Y * (PLAYER_HEIGHT * height);
                player_ray_clear(world, &nearby, center, sample)
            })
            .count() as f32
            / 3.0;
        if exposed > 0.0 {
            let weight = exposed * (1.0 - distance / radius).powi(2);
            visible.push((
                Target::Player(id),
                0,
                offset.try_normalize().unwrap_or(Vec3::Y),
                weight,
            ));
        }
    }
    // Normalize the material charge only. Player launch energy is bounded per actor.
    let normalizer = visible
        .iter()
        .filter(|(target, _, _, _)| !matches!(target, Target::Player(_)))
        .map(|(_, _, _, weight)| weight)
        .sum::<f32>()
        .max(1.0);
    visible
        .into_iter()
        .map(|(target, mat, direction, weight)| {
            let (kinetic_energy, dissipated_energy, force) = if matches!(target, Target::Player(_))
            {
                let energy = 0.5 * PLAYER_MASS * blast.player_speed.powi(2) * weight;
                (energy, 0.0, 0.0)
            } else {
                let energy = blast.energy * weight / normalizer;
                let kinetic = energy * (1.0 - blast.absorbed_fraction);
                let force = (2.0 * material(mat).density * kinetic).sqrt() / blast.load_window;
                (kinetic, energy * blast.absorbed_fraction, force)
            };
            BlastLoad {
                target,
                direction,
                kinetic_energy,
                player_damage: if matches!(target, Target::Player(_)) {
                    (MAX_PLAYER_BLAST_DAMAGE * weight).round() as u16
                } else {
                    0
                },
                contact: TerrainContact {
                    target: match target {
                        Target::Grid(cell) => cell.to_array(),
                        Target::Body(_) | Target::Player(_) => [0; 3],
                    },
                    material: mat,
                    dissipated_energy,
                    force,
                    area: 1.0,
                },
            }
        })
        .collect()
}

fn player_ray_clear(
    world: &VoxelWorld,
    bodies: &[&PhysicsBodySnapshot],
    center: Vec3,
    sample: Vec3,
) -> bool {
    let ray = sample - center;
    let reach = ray.length();
    let Some(aim) = ray.try_normalize() else {
        return world.block(center.floor().as_ivec3()) == Some(0);
    };
    world.raycast(center, aim, reach).is_none()
        && !bodies
            .iter()
            .any(|body| super::bow::ray_cube(center, aim, body.position, reach).is_some())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn empty_world() -> VoxelWorld {
        let mut world = VoxelWorld::default();
        world.insert(
            IVec3::ZERO,
            voxel_world::Chunk::from_runs(0, &[(32768, 0)]).unwrap(),
        );
        world
    }

    #[test]
    fn surface_blast_has_a_surviving_release_band_for_every_material() {
        let mut outcomes = Vec::new();
        for mat in 1..=5 {
            let mut world = empty_world();
            for y in 8..=9 {
                for z in 1..=11 {
                    for x in 1..=11 {
                        world.set_block(IVec3::new(x, y, z), mat).unwrap();
                    }
                }
            }
            let loads = plan(
                &world,
                &[],
                &[],
                Vec3::new(6.5, 10.02, 6.5),
                BowPower::Standard,
            );
            let mut destroyed = 0;
            let mut released = 0;
            for load in &loads {
                let Target::Grid(cell) = load.target else {
                    unreachable!()
                };
                assert_eq!(cell.y, 9, "the back layer must be shielded");
                assert!(
                    load.direction.y > 0.0,
                    "eject toward the exposed air above the floor"
                );
                assert!((load.direction.length_squared() - 1.0).abs() < 0.0001);
                let response = super::super::material_damage::response(
                    &load.contact,
                    0.0,
                    super::super::material_damage::attached_area(&world, cell),
                );
                destroyed += usize::from(response.destroyed);
                released += usize::from(response.detached);
            }
            let total: f32 = loads
                .iter()
                .map(|load| load.contact.dissipated_energy + load.kinetic_energy)
                .sum();
            assert!(total <= crate::packages::test_blast(BowPower::Standard).energy + 0.01);
            outcomes.push((mat, destroyed, released));
        }
        eprintln!("surface blast (material, destroyed, released): {outcomes:?}");
        assert!(
            outcomes
                .iter()
                .all(|&(_, destroyed, released)| destroyed > 0 && released >= 8),
            "each surface needs central fracture and a useful surviving ring: {outcomes:?}"
        );
    }

    #[test]
    fn blast_budget_is_shared_and_wall_occlusion_uses_pre_blast_state() {
        let mut world = empty_world();
        for y in 1..8 {
            for z in 1..8 {
                world.set_block(IVec3::new(6, y, z), 3).unwrap();
                world.set_block(IVec3::new(7, y, z), 3).unwrap();
            }
        }
        let plan = plan(
            &world,
            &[],
            &[],
            Vec3::new(5.98, 4.5, 4.5),
            BowPower::Standard,
        );
        assert!(plan.len() > 1);
        assert!(
            plan.iter()
                .all(|load| matches!(load.target, Target::Grid(cell) if cell.x == 6))
        );
        assert!(
            plan.iter().all(|load| load.direction.x < 0.0),
            "wall ejection must face air, rather than its backing layer"
        );
        let total: f32 = plan
            .iter()
            .map(|load| load.contact.dissipated_energy + load.kinetic_energy)
            .sum();
        assert!(total <= crate::packages::test_blast(BowPower::Standard).energy + 0.01);
        assert!(plan.iter().any(|load| material(3).damage_energy(
            load.contact.dissipated_energy,
            load.contact.force,
            load.contact.area
        ) >= 60.0));
        let mut damage = std::collections::HashMap::new();
        for load in plan {
            super::super::material_damage::apply_to_grid(
                &mut world,
                &mut damage,
                &load.contact,
                true,
            )
            .unwrap();
        }
        assert_eq!(world.block(IVec3::new(6, 4, 4)), Some(0));
        assert_eq!(world.block(IVec3::new(7, 4, 4)), Some(3));
        assert!(
            damage
                .values()
                .any(|state| state.release && state.joules < 60.0),
            "the blast should also release surviving surface blocks"
        );
    }

    #[test]
    fn outward_impulse_respects_energy_for_incoming_and_outgoing_bodies() {
        for mass in [0.7, 3.0] {
            for speed in [-30.0, 0.0, 30.0] {
                let velocity = Vec3::new(speed, 2.0, 0.0);
                let impulse = kinetic_impulse(mass, velocity, Vec3::X, 40.0);
                let after = velocity + impulse / mass;
                let added = 0.5 * mass * (after.length_squared() - velocity.length_squared());
                assert!((added - 40.0).abs() < 0.001, "added={added}");
                assert!(impulse.x > 0.0);
            }
        }
        let light = kinetic_impulse(0.7, Vec3::ZERO, Vec3::X, 40.0) / 0.7;
        let heavy = kinetic_impulse(3.0, Vec3::ZERO, Vec3::X, 40.0) / 3.0;
        assert!(light.length() > heavy.length());
    }

    #[test]
    fn loose_blocks_receive_work_and_shield_blocks_behind_them() {
        let world = empty_world();
        let near = PhysicsBodySnapshot {
            id: 1,
            position: Vec3::new(4., 4., 4.),
            velocity: Vec3::ZERO,
            material: 5,
        };
        let mut far = near;
        far.id = 2;
        far.position.x += 1.0;
        let loads = plan(
            &world,
            &[near, far],
            &[],
            Vec3::new(2.5, 4., 4.),
            BowPower::Standard,
        );
        assert_eq!(loads.len(), 1);
        assert_eq!(loads[0].target, Target::Body(1));
        assert!(loads[0].kinetic_energy > 0.0);
        assert!(
            plan(
                &world,
                &[near],
                &[],
                Vec3::new(12., 4., 4.),
                BowPower::Standard
            )
            .is_empty()
        );
        let inside = plan(&world, &[near, far], &[], near.position, BowPower::Standard);
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].target, Target::Body(near.id));
        assert!(inside[0].contact.dissipated_energy > 0.0);
        assert!(inside[0].direction.is_finite());
    }

    #[test]
    fn power_levels_scale_work_impulses_and_bounded_radius() {
        let world = empty_world();
        let body = PhysicsBodySnapshot {
            id: 1,
            position: Vec3::splat(10.0),
            velocity: Vec3::ZERO,
            material: 3,
        };
        let mut previous_force = 0.0;
        for power in BowPower::ALL {
            let loads = plan(&world, &[body], &[], body.position, power);
            assert_eq!(loads.len(), 1);
            let load = &loads[0];
            let spec = crate::packages::test_blast(power);
            let budget = spec.energy;
            assert!((load.contact.dissipated_energy + load.kinetic_energy - budget).abs() < 0.01);
            assert!(
                (load.contact.dissipated_energy - budget * spec.absorbed_fraction).abs() < 0.01
            );
            assert!(load.contact.force > previous_force);
            previous_force = load.contact.force;
            let outside = body.position - Vec3::X * (spec.radius + 0.1);
            assert!(plan(&world, &[body], &[], outside, power).is_empty());
            let inside = body.position - Vec3::X * (spec.radius - 0.1);
            assert_eq!(plan(&world, &[body], &[], inside, power).len(), 1);
        }
        let center = body.position - Vec3::X * 5.5;
        assert!(plan(&world, &[body], &[], center, BowPower::High).is_empty());
        assert_eq!(
            plan(&world, &[body], &[], center, BowPower::Extreme).len(),
            1
        );
    }

    #[test]
    fn player_knockback_is_not_diluted_by_terrain_or_other_players() {
        let mut world = empty_world();
        for x in 7..=13 {
            for z in 7..=13 {
                world.set_block(IVec3::new(x, 9, z), 3).unwrap();
            }
        }
        let player = PlayerState {
            position: Vec3::new(10.5, 10.0, 10.5),
            ..Default::default()
        };
        let players = [(1, player), (2, player)];
        let center = Vec3::new(10.5, 10.02, 10.5);
        let mut previous = 0.0;
        for power in BowPower::ALL {
            let loads = plan(&world, &[], &players, center, power);
            let first = loads
                .iter()
                .find(|load| load.target == Target::Player(1))
                .unwrap();
            let second = loads
                .iter()
                .find(|load| load.target == Target::Player(2))
                .unwrap();
            assert_eq!(first.kinetic_energy, second.kinetic_energy);
            let solo = plan(&empty_world(), &[], &players[..1], center, power);
            let solo = solo
                .iter()
                .find(|load| load.target == Target::Player(1))
                .unwrap();
            assert_eq!(first.kinetic_energy, solo.kinetic_energy);
            let spec = crate::packages::test_blast(power);
            let cap = 0.5 * PLAYER_MASS * spec.player_speed.powi(2);
            assert!(first.kinetic_energy <= cap);
            assert_eq!(first.contact.dissipated_energy, 0.0);
            assert!(first.direction.y > 0.99);
            assert!(first.kinetic_energy > previous);
            previous = first.kinetic_energy;
            let total: f32 = loads
                .iter()
                .filter(|load| !matches!(load.target, Target::Player(_)))
                .map(|load| load.kinetic_energy + load.contact.dissipated_energy)
                .sum();
            assert!(total <= spec.energy + 0.01);
            assert!(
                loads
                    .iter()
                    .any(|load| matches!(load.target, Target::Grid(_)))
            );
        }
    }

    #[test]
    fn players_obey_range_noclip_and_terrain_and_body_shielding() {
        let mut world = empty_world();
        let player = PlayerState {
            position: Vec3::new(10.5, 10.0, 10.5),
            ..Default::default()
        };
        let center = Vec3::new(8.5, 10.9, 10.5);
        let players = [(1, player)];
        assert_eq!(
            plan(&world, &[], &players, center, BowPower::Standard).len(),
            1
        );
        assert!(
            plan(
                &world,
                &[],
                &players,
                center - Vec3::X * 5.0,
                BowPower::Standard
            )
            .is_empty()
        );
        let flying = PlayerState {
            noclip: true,
            ..player
        };
        assert!(plan(&world, &[], &[(1, flying)], center, BowPower::Standard).is_empty());
        let body = PhysicsBodySnapshot {
            id: 1,
            position: Vec3::new(9.5, 10.9, 10.5),
            velocity: Vec3::ZERO,
            material: 3,
        };
        assert!(
            plan(&world, &[body], &players, center, BowPower::Standard)
                .iter()
                .all(|load| load.target != Target::Player(1))
        );
        for y in 9..=12 {
            world.set_block(IVec3::new(9, y, 10), 3).unwrap();
        }
        assert!(
            plan(&world, &[], &players, center, BowPower::Standard)
                .iter()
                .all(|load| load.target != Target::Player(1))
        );
    }

    #[test]
    fn player_damage_uses_falloff_and_partial_exposure_without_sharing_a_budget() {
        let mut world = empty_world();
        let center = Vec3::new(8.5, 10.9, 10.5);
        let player = PlayerState {
            position: Vec3::new(10.5, 10.0, 10.5),
            ..Default::default()
        };
        let damage = |world: &VoxelWorld, players: &[(u64, PlayerState)]| {
            plan(world, &[], players, center, BowPower::Standard)
                .into_iter()
                .filter(|load| matches!(load.target, Target::Player(_)))
                .map(|load| load.player_damage)
                .collect::<Vec<_>>()
        };
        let full = damage(&world, &[(1, player)])[0];
        assert!(full > 0);
        assert_eq!(damage(&world, &[(1, player), (2, player)]), [full, full]);
        let farther = PlayerState {
            position: player.position + Vec3::X,
            ..player
        };
        assert!(damage(&world, &[(1, farther)])[0] < full);
        // The top ray clears this block, the lower two do not.
        world.set_block(IVec3::new(9, 10, 10), 3).unwrap();
        let partial = damage(&world, &[(1, player)])[0];
        assert!(partial > 0 && partial < full);
        world.set_block(IVec3::new(9, 11, 10), 3).unwrap();
        assert!(damage(&world, &[(1, player)]).is_empty());
    }
}
