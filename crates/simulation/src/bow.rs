//! Authoritative point-projectile flight with swept terrain and loose-cube hits.
use glam::Vec3;
use protocol::{ArrowSnapshot, PhysicsBodySnapshot};
use voxel_world::VoxelWorld;

const ARROW_SPEED: f32 = 36.0;
const ARROW_GRAVITY: f32 = 3.0;
const MAX_TRAVEL: f32 = 64.0;
const MAX_AGE_TICKS: u32 = 180;

pub(super) struct Arrow {
    pub snapshot: ArrowSnapshot,
    age: u32,
    traveled: f32,
    pending_steps: u32,
}

pub(super) enum Flight {
    Flying,
    Impact(Vec3),
    Expired,
}

impl Arrow {
    pub fn new(id: u32, origin: Vec3, direction: Vec3) -> Self {
        Self {
            snapshot: ArrowSnapshot {
                id,
                position: origin,
                velocity: direction * ARROW_SPEED,
            },
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
        ready: bool,
    ) -> Flight {
        self.age += 1;
        if self.age > MAX_AGE_TICKS {
            return Flight::Expired;
        }
        self.pending_steps = (self.pending_steps + 1).min(3);
        if ready {
            for _ in 0..std::mem::take(&mut self.pending_steps) {
                let result = self.step(world, bodies);
                if !matches!(result, Flight::Flying) {
                    return result;
                }
            }
        }
        Flight::Flying
    }

    fn step(&mut self, world: &VoxelWorld, bodies: &[PhysicsBodySnapshot]) -> Flight {
        if self.traveled >= MAX_TRAVEL {
            return Flight::Expired;
        }
        let start = self.snapshot.position;
        // Expire at the loaded-world boundary rather than firing through unknown terrain.
        if world.block(start.floor().as_ivec3()).is_none() {
            return Flight::Expired;
        }
        self.snapshot.velocity.y -= ARROW_GRAVITY * physics::FIXED_DT;
        let displacement = self.snapshot.velocity * physics::FIXED_DT;
        let distance = displacement.length().min(MAX_TRAVEL - self.traveled);
        let direction = displacement.normalize();
        let mut impact = world
            .raycast(start, direction, distance)
            .map(|hit| hit.distance);
        for body in bodies {
            if let Some(hit) = ray_cube(start, direction, body.position, distance)
                && impact.is_none_or(|old| hit < old)
            {
                impact = Some(hit);
            }
        }
        if let Some(hit) = impact {
            // Detonate just outside the surface so the impacted face and its
            // neighboring exposed faces can receive blast work.
            return Flight::Impact(start + direction * (hit - 0.02).max(0.0));
        }
        self.snapshot.position += direction * distance;
        self.traveled += distance;
        if world
            .block(self.snapshot.position.floor().as_ivec3())
            .is_none()
        {
            Flight::Expired
        } else {
            Flight::Flying
        }
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
        let mut arrow = Arrow::new(7, Vec3::new(2.5, 10.5, 2.5), Vec3::X);
        for _ in 0..100 {
            match arrow.step(&world, &[]) {
                Flight::Flying => {}
                Flight::Impact(position) => {
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
        let mut arrow = Arrow::new(1, Vec3::new(2.9, 5.5, 2.5), Vec3::X);
        let Flight::Impact(position) = arrow.step(&world, &[body]) else {
            panic!("missed body");
        };
        assert!((position.x - 2.98).abs() < 0.001);
        assert_eq!(
            ray_cube(Vec3::ZERO, Vec3::X, Vec3::new(2., 2., 0.), 4.),
            None
        );
    }

    #[test]
    fn arrows_expire_at_range_and_loaded_boundary() {
        let world = empty_world();
        let mut arrow = Arrow::new(1, Vec3::new(1., 20., 1.), Vec3::X);
        for _ in 0..180 {
            if matches!(arrow.step(&world, &[]), Flight::Expired) {
                assert!(arrow.snapshot.position.x <= 65.0 && arrow.snapshot.position.x > 64.0);
                break;
            }
        }
        assert!(arrow.traveled >= MAX_TRAVEL);
        let mut arrow = Arrow::new(2, Vec3::new(95.9, 10., 1.), Vec3::X);
        assert!(matches!(arrow.step(&world, &[]), Flight::Expired));
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
        let mut arrow = Arrow::new(1, Vec3::new(2.9, 5.5, 2.5), Vec3::X);
        assert!(matches!(
            arrow.tick(&world, &[stale], false),
            Flight::Flying
        ));
        assert_eq!(arrow.snapshot.position.x, 2.9);
        assert!(matches!(arrow.tick(&world, &[], true), Flight::Flying));
        assert!((arrow.snapshot.position.x - 4.1).abs() < 0.001);
        for _ in 0..MAX_AGE_TICKS {
            arrow.tick(&world, &[], false);
        }
        assert!(matches!(arrow.tick(&world, &[], false), Flight::Expired));
        assert!(arrow.pending_steps <= 3);
    }
}
