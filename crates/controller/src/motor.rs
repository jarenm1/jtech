use crate::{CharacterBody, CharacterIntent, CharacterState, MovementProfile, finite};
use glam::{Vec2, Vec3};
use physics::DynamicCollider;
use voxel_world::VoxelWorld;

/// Advance one bounded physics tick. Movement and turn persist; jump is consumed.
/// Callers own observation timing, fixed tick scheduling, and snapshot selection.
pub fn step_character(
    world: &VoxelWorld,
    character: &mut CharacterState,
    body: &CharacterBody,
    profile: &MovementProfile,
    intent: &mut CharacterIntent,
    dt: f32,
    bodies: &[DynamicCollider],
) {
    if !dt.is_finite() || dt <= 0.0 {
        return;
    }
    let input = intent.bounded();
    intent.jump = false;
    let profile = profile.bounded();
    character.yaw = (finite(character.yaw) + input.turn * profile.turn_rate * dt.min(0.25))
        .rem_euclid(std::f32::consts::TAU);
    let state = &mut character.motion;
    // Noclip is exclusively an adapter concern. Generic actors always collide.
    state.noclip = false;
    if !state.position.is_finite() || state.position.abs().max_element() > 32_000_000.0 {
        *state = physics::KinematicState::default();
    }
    if !state.velocity.is_finite() {
        state.velocity = Vec3::ZERO;
    }
    state.external_velocity = physics::bounded_horizontal(state.external_velocity);
    // Bounded catch-up prevents pathological caller timesteps and unbounded collision work.
    let dt = dt.min(0.25);
    physics::separate_bodies(world, state, bodies, body.shape);
    let mut probe = state.position;
    state.grounded = state.velocity.y <= 0.0
        && physics::sweep_with_bodies(world, &mut probe, 1, -0.002, bodies, None, body.shape);
    let yaw = finite(character.yaw);
    let (sy, cy) = yaw.sin_cos();
    let right = Vec3::new(cy, 0.0, -sy);
    let forward = Vec3::new(-sy, 0.0, -cy);
    let movement = glam::Vec2::new(
        finite(input.movement.x * profile.strafe).clamp(-1.0, 1.0),
        finite(input.movement.y).clamp(-1.0, 1.0),
    )
    .clamp_length_max(1.0);
    let horizontal = (right * movement.x + forward * movement.y) * profile.speed;
    let target = Vec2::new(horizontal.x, horizontal.z);
    let previous = physics::bounded_horizontal(
        Vec2::new(state.velocity.x, state.velocity.z) - state.external_velocity,
    );
    let rate = if target.length_squared() < previous.length_squared() {
        profile.braking
    } else {
        profile.acceleration
    };
    let control = if state.grounded {
        1.0
    } else {
        profile.air_control
    };
    let horizontal = previous
        + (target - previous).clamp_length_max(rate * control * dt)
        + state.external_velocity;
    state.velocity.x = horizontal.x;
    state.velocity.z = horizontal.y;
    if input.jump && profile.jump_speed > 0.0 && state.grounded {
        state.velocity.y = profile.jump_speed;
        state.grounded = false;
    }
    state.velocity.y = (state.velocity.y - profile.gravity * dt).clamp(-60.0, 60.0);
    if physics::sweep_with_bodies(
        world,
        &mut state.position,
        0,
        state.velocity.x * dt,
        bodies,
        None,
        body.shape,
    ) {
        state.velocity.x = 0.0;
        state.external_velocity.x = 0.0;
    }
    if physics::sweep_with_bodies(
        world,
        &mut state.position,
        2,
        state.velocity.z * dt,
        bodies,
        None,
        body.shape,
    ) {
        state.velocity.z = 0.0;
        state.external_velocity.y = 0.0;
    }
    let upward = state.velocity.y > 0.0;
    // Grounded actors snap down a full step so descending slopes keep support.
    let snap = state.grounded && state.velocity.y <= 0.0;
    let fall = if snap {
        (state.velocity.y * dt).min(-physics::STEP_HEIGHT)
    } else {
        state.velocity.y * dt
    };
    if physics::sweep_with_bodies(
        world,
        &mut state.position,
        1,
        fall,
        bodies,
        None,
        body.shape,
    ) {
        state.velocity.y = 0.0;
        state.grounded = !upward;
    } else {
        // Zero gravity still has support; probe again after horizontal travel so
        // walking off an edge clears grounded during this same tick.
        let mut probe = state.position;
        state.grounded = state.velocity.y == 0.0
            && physics::sweep_with_bodies(world, &mut probe, 1, -0.002, bodies, None, body.shape);
    }
    // Exponential drag gives the same momentum decay across different tick sizes.
    let drag = if state.grounded {
        profile.ground_drag
    } else {
        profile.air_drag
    };
    let previous = state.external_velocity;
    state.external_velocity *= (-drag * dt).exp();
    let change = state.external_velocity - previous;
    state.velocity.x += change.x;
    state.velocity.z += change.y;
}
