//! Bounded blast allocation against one pre-explosion world observation.
//! Reserve separate absorbed-work and kinetic budgets; occluded targets receive neither.
use glam::{IVec3, Vec3};
use gpu_physics::{TerrainContact, material};
use protocol::PhysicsBodySnapshot;
use voxel_world::{CHUNK_SIZE, MIN_CHUNK_Y, VoxelWorld};

pub(super) const RADIUS: f32 = 4.0;
const ENERGY: f32 = 6000.0;
const ABSORBED_FRACTION: f32 = 0.35;
// A short pressure pulse can break surface attachments before all receivers fracture.
const LOAD_WINDOW: f32 = 0.00075;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Target {
    Grid(IVec3),
    Body(u32),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct BlastLoad {
    pub target: Target,
    pub contact: TerrainContact,
    pub direction: Vec3,
    pub kinetic_energy: f32,
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

/// Sample facing cube-face centers, with 1m² effective area and radial falloff.
/// Freeze the receivers before changing terrain, so removing the front of a wall
/// cannot expose its back to the same explosion.
pub(super) fn plan(
    world: &VoxelWorld,
    bodies: &[PhysicsBodySnapshot],
    center: Vec3,
) -> Vec<BlastLoad> {
    if !center.is_finite() {
        return Vec::new();
    }
    let min = (center - Vec3::splat(RADIUS)).floor().as_ivec3();
    let max = (center + Vec3::splat(RADIUS)).floor().as_ivec3();
    let mut candidates = Vec::new();
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
    for body in bodies {
        candidates.push((Target::Body(body.id), body.position, body.material as u32));
    }
    let mut visible = Vec::new();
    for (target, position, mat) in candidates {
        let offset = position - center;
        let distance = offset.length();
        if distance >= RADIUS {
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
                Target::Body(_) => terrain_hit.is_none(),
            };
            if terrain_clear
                && !bodies.iter().any(|body| {
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
        let falloff = (1.0 - distance / RADIUS).powi(2);
        let weight = falloff / (distance * distance + 0.25);
        visible.push((target, mat, direction, weight));
    }
    // Leave low-coverage energy in the air; never distribute more than one charge.
    let normalizer = visible
        .iter()
        .map(|(_, _, _, weight)| weight)
        .sum::<f32>()
        .max(1.0);
    visible
        .into_iter()
        .map(|(target, mat, direction, weight)| {
            let energy = ENERGY * weight / normalizer;
            let kinetic_energy = energy * (1.0 - ABSORBED_FRACTION);
            let force = (2.0 * material(mat).density * kinetic_energy).sqrt() / LOAD_WINDOW;
            BlastLoad {
                target,
                direction,
                kinetic_energy,
                contact: TerrainContact {
                    target: match target {
                        Target::Grid(cell) => cell.to_array(),
                        Target::Body(_) => [0; 3],
                    },
                    material: mat,
                    dissipated_energy: energy * ABSORBED_FRACTION,
                    force,
                    area: 1.0,
                },
            }
        })
        .collect()
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
            let loads = plan(&world, &[], Vec3::new(6.5, 10.02, 6.5));
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
            assert!(total <= ENERGY + 0.01);
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
        let plan = plan(&world, &[], Vec3::new(5.98, 4.5, 4.5));
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
        assert!(total <= ENERGY + 0.01);
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
        let loads = plan(&world, &[near, far], Vec3::new(2.5, 4., 4.));
        assert_eq!(loads.len(), 1);
        assert_eq!(loads[0].target, Target::Body(1));
        assert!(loads[0].kinetic_energy > 0.0);
        assert!(plan(&world, &[near], Vec3::new(12., 4., 4.)).is_empty());
        let inside = plan(&world, &[near, far], near.position);
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].target, Target::Body(near.id));
        assert!(inside[0].contact.dissipated_energy > 0.0);
        assert!(inside[0].direction.is_finite());
    }
}
