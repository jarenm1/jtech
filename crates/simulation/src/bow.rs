//! Authoritative point-projectile flight with swept terrain and loose-cube hits.
use crate::actors::SimEntity;
use game_packages::{BlastSpec, ProjectileSpec};
use gameplay::combat::SwingTarget;
use glam::Vec3;
use protocol::{ArrowSnapshot, PhysicsBodySnapshot};
#[cfg(test)]
use protocol::BowPower;
use voxel_world::VoxelWorld;

pub(super) struct Arrow {
    pub snapshot: ArrowSnapshot,
    /// Flight parameters, independent of the package that authored them.
    pub projectile: ProjectileSpec,
    /// Blast applied on impact; `None` makes a plain arrow that damages the
    /// character it strikes directly instead of exploding.
    pub blast: Option<BlastSpec>,
    /// Whole health points a plain arrow removes from the character it hits.
    pub damage: u16,
    /// Impulse in kg·m/s a plain arrow applies along its flight direction.
    pub knockback: f32,
    /// Entity that fired the arrow; kept on queued detonations for event
    /// attribution.
    pub shooter: Option<SimEntity>,
    age: u32,
    traveled: f32,
    pending_steps: u32,
}

pub(super) enum Flight {
    Flying,
    /// Impact at a point, with the swing-target id struck if the arrow hit a
    /// character rather than terrain or a loose body.
    Impact(Vec3, Option<u64>),
    Expired,
}

impl Arrow {
    #[cfg(test)]
    pub fn new(id: u32, origin: Vec3, direction: Vec3, power: BowPower) -> Self {
        let shot = game_packages::PackageHost::new(&game_packages::default_directory())
            .fire(power)
            .unwrap();
        Self::new_arrow(id, origin, direction, shot.projectile, Some(shot.impact()))
    }

    /// Build an arrow from explicit flight and impact parameters. `blast: None`
    /// makes a plain arrow that damages the character it strikes directly.
    pub fn new_arrow(
        id: u32,
        origin: Vec3,
        direction: Vec3,
        projectile: ProjectileSpec,
        blast: Option<BlastSpec>,
    ) -> Self {
        Self {
            snapshot: ArrowSnapshot {
                id,
                position: origin,
                velocity: direction * projectile.speed,
            },
            projectile,
            blast,
            damage: 0,
            knockback: 0.0,
            shooter: None,
            age: 0,
            traveled: 0.0,
            pending_steps: 0,
        }
    }

    /// Hold collision work until current GPU body observations are available.
    /// Catch up at most 50ms; under sustained overload, projectile motion slows
    /// with body simulation, while wall-tick expiry still bounds its lifetime.
    pub fn tick(
        &mut self,
        world: &VoxelWorld,
        bodies: &[PhysicsBodySnapshot],
        targets: &[SwingTarget],
        ready: bool,
    ) -> Flight {
        self.age += 1;
        if self.age > self.projectile.max_age_ticks {
            return Flight::Expired;
        }
        self.pending_steps = (self.pending_steps + 1).min(3);
        if ready {
            for _ in 0..std::mem::take(&mut self.pending_steps) {
                let result = self.step(world, bodies, targets);
                if !matches!(result, Flight::Flying) {
                    return result;
                }
            }
        }
        Flight::Flying
    }

    fn step(
        &mut self,
        world: &VoxelWorld,
        bodies: &[PhysicsBodySnapshot],
        targets: &[SwingTarget],
    ) -> Flight {
        if self.traveled >= self.projectile.max_travel {
            return Flight::Expired;
        }
        let start = self.snapshot.position;
        self.snapshot.velocity.y -= self.projectile.gravity * physics::FIXED_DT;
        let displacement = self.snapshot.velocity * physics::FIXED_DT;
        let distance = displacement
            .length()
            .min(self.projectile.max_travel - self.traveled);
        let Some(direction) = displacement.try_normalize() else {
            // Authored gravity can bring an upward shot momentarily to rest.
            return Flight::Flying;
        };
        let mut impact = world
            .raycast(start, direction, distance)
            .map(|hit| hit.distance);
        let mut hit_target = None;
        for body in bodies {
            if let Some(hit) = ray_cube(start, direction, body.position, distance)
                && impact.is_none_or(|old| hit < old)
            {
                impact = Some(hit);
                hit_target = None;
            }
        }
        for target in targets {
            if let Some(hit) = physics::raycast_body(start, direction, target.position, target.shape)
                && hit <= distance
                && impact.is_none_or(|old| hit < old)
            {
                impact = Some(hit);
                hit_target = Some(target.id);
            }
        }
        if let Some(hit) = impact {
            // Detonate just outside the surface so the impacted face and its
            // neighboring exposed faces can receive blast work.
            return Flight::Impact(start + direction * (hit - 0.02).max(0.0), hit_target);
        }
        self.snapshot.position += direction * distance;
        self.traveled += distance;
        // An arrow may leave the loaded world and fall back into it: terrain
        // only blocks it where chunks are loaded, and the age cap bounds it.
        Flight::Flying
    }
}

/// Distance to entry into a unit cube, including a zero-distance starting overlap.
pub(super) fn ray_cube(origin: Vec3, direction: Vec3, center: Vec3, limit: f32) -> Option<f32> {
    let mut near: f32 = 0.0;
    let mut far = limit;
    for axis in 0..3 {
        let low = center[axis] - 0.5;
        let high = center[axis] + 0.5;
        if direction[axis].abs() < 1e-6 {
            if origin[axis] < low || origin[axis] > high {
                return None;
            }
        } else {
            let a = (low - origin[axis]) / direction[axis];
            let b = (high - origin[axis]) / direction[axis];
            near = near.max(a.min(b));
            far = far.min(a.max(b));
            if near > far {
                return None;
            }
        }
    }
    Some(near)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::IVec3;

    pub(super) fn empty_world() -> VoxelWorld {
        let mut world = VoxelWorld::default();
        for x in 0..3 {
            world.insert(
                IVec3::new(x, 0, 0),
                voxel_world::Chunk::from_runs(0, &[(32768, 0)]).unwrap(),
            );
        }
        world
    }

    #[test]
    fn arrow_flies_beyond_edit_reach_and_hits_first_wall_once() {
        let mut world = empty_world();
        for y in 0..16 {
            world.set_block(IVec3::new(20, y, 2), 3).unwrap();
        }
        let mut arrow = Arrow::new(7, Vec3::new(2.5, 10.5, 2.5), Vec3::X, BowPower::Standard);
        for _ in 0..100 {
            match arrow.step(&world, &[], &[]) {
                Flight::Flying => {}
                Flight::Impact(position, _) => {
                    assert!((position.x - 19.98).abs() < 0.001);
                    assert!(position.y < 10.5);
                    return;
                }
                Flight::Expired => panic!("arrow expired before wall"),
            }
        }
        panic!("arrow tunneled through wall");
    }

    #[test]
    fn swept_arrow_hits_loose_body_before_terrain() {
        let world = empty_world();
        let body = PhysicsBodySnapshot {
            id: 1,
            position: Vec3::new(3.5, 5.5, 2.5),
            velocity: Vec3::ZERO,
            material: 3,
        };
        let mut arrow = Arrow::new(1, Vec3::new(2.9, 5.5, 2.5), Vec3::X, BowPower::Standard);
        let Flight::Impact(position, _) = arrow.step(&world, &[body], &[]) else {
            panic!("missed body");
        };
        assert!((position.x - 2.98).abs() < 0.001);
        assert_eq!(
            ray_cube(Vec3::ZERO, Vec3::X, Vec3::new(2., 2., 0.), 4.),
            None
        );
    }

    #[test]
    fn arrows_fly_past_the_loaded_boundary_and_expire_on_age() {
        let world = empty_world();
        // An arrow leaving the loaded world keeps flying, so a shot straight up
        // can fall back into loaded terrain.
        let mut arrow = Arrow::new(1, Vec3::new(95.9, 10., 1.), Vec3::X, BowPower::Standard);
        assert!(matches!(arrow.step(&world, &[], &[]), Flight::Flying));
        assert!(arrow.snapshot.position.x > 96.0);
        // The age cap still bounds its lifetime.
        for _ in 0..arrow.projectile.max_age_ticks {
            arrow.tick(&world, &[], &[], false);
        }
        assert!(matches!(arrow.tick(&world, &[], &[], false), Flight::Expired));
    }

    #[test]
    fn a_vertical_arrow_falls_back_into_the_loaded_world() {
        let world = empty_world();
        let mut arrow = Arrow::new(1, Vec3::new(48.5, 20.5, 48.5), Vec3::Y, BowPower::Standard);
        let origin = arrow.snapshot.position;
        let mut apex = origin.y;
        for _ in 0..1800 {
            let _ = arrow.step(&world, &[], &[]);
            apex = apex.max(arrow.snapshot.position.y);
            if arrow.snapshot.position.y <= origin.y && arrow.snapshot.velocity.y < 0.0 {
                break;
            }
        }
        assert!(apex > origin.y + 100.0, "arrow did not climb: {apex}");
        assert!(arrow.snapshot.position.y <= origin.y + 0.5);
    }

    #[test]
    fn arrows_expire_at_their_travel_cap() {
        let world = empty_world();
        let spec = ProjectileSpec {
            speed: 36.0,
            gravity: 3.0,
            max_travel: 8.0,
            max_age_ticks: 18_000,
        };
        let mut arrow = Arrow::new_arrow(1, Vec3::new(1., 20., 1.), Vec3::X, spec, None);
        for _ in 0..180 {
            if matches!(arrow.step(&world, &[], &[]), Flight::Expired) {
                assert!(arrow.snapshot.position.x <= 9.0 && arrow.snapshot.position.x > 8.0);
                break;
            }
        }
        assert!(arrow.traveled >= arrow.projectile.max_travel);
    }

    #[test]
    fn busy_gpu_defers_collision_to_fresh_poses_with_bounded_catchup() {
        let world = empty_world();
        let stale = PhysicsBodySnapshot {
            id: 1,
            position: Vec3::new(3.5, 5.5, 2.5),
            velocity: Vec3::ZERO,
            material: 3,
        };
        let mut arrow = Arrow::new(1, Vec3::new(2.9, 5.5, 2.5), Vec3::X, BowPower::Standard);
        assert!(matches!(
            arrow.tick(&world, &[stale], &[], false),
            Flight::Flying
        ));
        assert_eq!(arrow.snapshot.position.x, 2.9);
        assert!(matches!(arrow.tick(&world, &[], &[], true), Flight::Flying));
        assert!((arrow.snapshot.position.x - 4.1).abs() < 0.001);
        for _ in 0..arrow.projectile.max_age_ticks {
            arrow.tick(&world, &[], &[], false);
        }
        assert!(matches!(arrow.tick(&world, &[], &[], false), Flight::Expired));
        assert!(arrow.pending_steps <= 3);
    }
}
