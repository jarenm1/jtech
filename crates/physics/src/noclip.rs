use super::{
    DynamicCollider, EPSILON, PlayerInput, PlayerState, bounds, finite, look_direction, solid,
};
use glam::{IVec3, Vec3};
use voxel_world::VoxelWorld;

const FLY_SPEED: f32 = 12.0;

/// Handle flight before collision/gravity. Exit only with room for the whole actor.
pub(super) fn step(
    world: &VoxelWorld,
    state: &mut PlayerState,
    input: &PlayerInput,
    dt: f32,
    bodies: &[DynamicCollider],
) -> bool {
    if input.noclip {
        state.noclip = true;
    } else if state.noclip && clear(world, state.position, bodies) {
        state.noclip = false;
        state.velocity = Vec3::ZERO;
    }
    if !state.noclip {
        return false;
    }
    let (sy, cy) = finite(input.yaw).sin_cos();
    let right = Vec3::new(cy, 0.0, -sy);
    let forward = look_direction(input.yaw, input.pitch);
    let vertical = f32::from(u8::from(input.jump)) - f32::from(u8::from(input.descend));
    let direction = right * finite(input.movement[0]).clamp(-1.0, 1.0)
        + forward * finite(input.movement[1]).clamp(-1.0, 1.0)
        + Vec3::Y * vertical;
    state.velocity = direction.clamp_length_max(1.0) * FLY_SPEED;
    state.position += state.velocity * dt;
    state.grounded = false;
    true
}

fn clear(world: &VoxelWorld, position: Vec3, bodies: &[DynamicCollider]) -> bool {
    let (min, max) = bounds(position);
    let first = (min + Vec3::splat(EPSILON)).floor().as_ivec3();
    let last = (max - Vec3::splat(EPSILON)).floor().as_ivec3();
    for y in first.y..=last.y {
        for z in first.z..=last.z {
            for x in first.x..=last.x {
                if solid(world, IVec3::new(x, y, z)) {
                    return false;
                }
            }
        }
    }
    !bodies.iter().any(|body| {
        body.position.is_finite()
            && min.cmplt(body.position + Vec3::splat(0.5 - EPSILON)).all()
            && max.cmpgt(body.position - Vec3::splat(0.5 - EPSILON)).all()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FIXED_DT, step_player_with_bodies};

    #[test]
    fn flight_ignores_solids_bodies_and_gravity_and_stops_when_released() {
        let world = crate::tests::arena();
        let mut state = PlayerState {
            position: Vec3::new(0.5, -2.0, 0.5),
            ..Default::default()
        };
        let bodies = [DynamicCollider {
            id: 1,
            position: state.position,
            velocity: Vec3::ZERO,
        }];
        let mut input = PlayerInput {
            noclip: true,
            jump: true,
            ..Default::default()
        };
        for _ in 0..60 {
            step_player_with_bodies(&world, &mut state, &input, FIXED_DT, &bodies);
        }
        assert!((state.position.y - 10.0).abs() < 0.001);
        assert!(state.noclip);
        assert!(!state.grounded);
        input.jump = false;
        let position = state.position;
        step_player_with_bodies(&world, &mut state, &input, FIXED_DT, &bodies);
        assert_eq!(state.position, position);
        assert_eq!(state.velocity, Vec3::ZERO);
        input.descend = true;
        step_player_with_bodies(&world, &mut state, &input, FIXED_DT, &bodies);
        assert!(state.position.y < position.y);
    }

    #[test]
    fn flight_is_view_relative_speed_bounded_and_crosses_unloaded_space() {
        let world = VoxelWorld::default();
        let mut state = PlayerState::default();
        let input = PlayerInput {
            noclip: true,
            movement: [1.0, 1.0],
            pitch: 0.5,
            jump: true,
            ..Default::default()
        };
        step_player_with_bodies(&world, &mut state, &input, FIXED_DT, &[]);
        assert!((state.velocity.length() - FLY_SPEED).abs() < 0.001);
        assert!(state.velocity.x > 0.0 && state.velocity.y > 0.0 && state.velocity.z < 0.0);
        let input = PlayerInput {
            noclip: true,
            movement: [f32::NAN, f32::INFINITY],
            yaw: f32::NAN,
            pitch: f32::INFINITY,
            ..Default::default()
        };
        step_player_with_bodies(&world, &mut state, &input, FIXED_DT, &[]);
        assert!(state.position.is_finite());
        assert_eq!(state.velocity, Vec3::ZERO);
    }

    #[test]
    fn exiting_requires_clear_terrain_and_bodies_then_restores_gravity() {
        let world = crate::tests::arena();
        let mut state = PlayerState {
            position: Vec3::new(0.5, -1.0, 0.5),
            noclip: true,
            ..Default::default()
        };
        let input = PlayerInput::default();
        step_player_with_bodies(&world, &mut state, &input, FIXED_DT, &[]);
        assert!(state.noclip);
        state.position.y = 4.0;
        let bodies = [DynamicCollider {
            id: 1,
            position: state.position,
            velocity: Vec3::ZERO,
        }];
        step_player_with_bodies(&world, &mut state, &input, FIXED_DT, &bodies);
        assert!(state.noclip);
        step_player_with_bodies(&world, &mut state, &input, FIXED_DT, &[]);
        assert!(!state.noclip);
        assert!(state.velocity.y < 0.0);
        assert!(state.position.y < 4.0);
    }
}
