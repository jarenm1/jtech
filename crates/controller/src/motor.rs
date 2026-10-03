use crate::{CharacterBody, CharacterIntent, CharacterState, MovementProfile, finite};
use glam::{Vec2, Vec3};
use physics::DynamicCollider;
use voxel_world::VoxelWorld;

/// Shared per-tick inputs and derived values threaded through the motor stages.
/// Stages run in a fixed order so authority, prediction and replay agree.
struct Motor<'a> {
    world: &'a VoxelWorld,
    bodies: &'a physics::BodyBroadphase<'a>,
    body: &'a CharacterBody,
    profile: &'a MovementProfile,
    input: CharacterIntent,
    dt: f32,
    /// Horizontal velocity the character attempted this tick, before terrain
    /// and loose bodies clamped the axis sweeps.
    attempted: Vec2,
}

/// Advance one bounded physics tick. Movement and turn persist; jump is consumed.
/// Callers own observation timing, fixed tick scheduling, and snapshot selection.
/// Returns the horizontal velocity the character attempted this tick: input
/// acceleration plus external impulse, before terrain or bodies clamp motion.
pub fn step_character(
    world: &VoxelWorld,
    character: &mut CharacterState,
    body: &CharacterBody,
    profile: &MovementProfile,
    intent: &mut CharacterIntent,
    dt: f32,
    bodies: &[DynamicCollider],
) -> Vec2 {
    if !dt.is_finite() || dt <= 0.0 {
        return Vec2::new(character.motion.velocity.x, character.motion.velocity.z);
    }
    let profile = profile.bounded();
    let input = intent.bounded();
    intent.jump = false;
    // One grid build amortizes this tick's separation pass and every sweep.
    let broadphase = physics::BodyBroadphase::new(bodies);
    let mut motor = Motor {
        world,
        bodies: &broadphase,
        body,
        profile: &profile,
        input,
        // Bounded catch-up prevents pathological caller timesteps and unbounded collision work.
        dt: dt.min(0.25),
        attempted: Vec2::ZERO,
    };
    sanitize(&mut motor, character);
    contact(&mut motor, character);
    steer(&mut motor, character);
    gravity(&mut motor, character);
    sweep_horizontal(&mut motor, character);
    sweep_vertical(&mut motor, character);
    drag(&mut motor, character);
    motor.attempted
}

/// Bound the tick and the replay state, and advance yaw from held turn.
fn sanitize(motor: &mut Motor, character: &mut CharacterState) {
    let state = &mut character.motion;
    character.yaw = (finite(character.yaw) + motor.input.turn * motor.profile.turn_rate * motor.dt)
        .rem_euclid(std::f32::consts::TAU);
    // Noclip is exclusively an adapter concern. Generic actors always collide.
    state.noclip = false;
    if !state.position.is_finite() || state.position.abs().max_element() > 32_000_000.0 {
        *state = physics::KinematicState::default();
    }
    if !state.velocity.is_finite() {
        state.velocity = Vec3::ZERO;
    }
    state.external_velocity = physics::bounded_horizontal(state.external_velocity);
}

/// Repair snapshot overlap, then refresh support with a shallow downward probe.
fn contact(motor: &mut Motor, character: &mut CharacterState) {
    let state = &mut character.motion;
    physics::separate_bodies(motor.world, state, motor.bodies, motor.body.shape);
    let mut probe = state.position;
    state.grounded = state.velocity.y <= 0.0
        && physics::sweep_with_bodies(
            motor.world,
            &mut probe,
            1,
            -0.002,
            motor.bodies,
            None,
            motor.body.shape,
        );
}

/// Turn held body-relative input into a target horizontal velocity and
/// accelerate or brake toward it, keeping external momentum separate.
fn steer(motor: &mut Motor, character: &mut CharacterState) {
    let state = &mut character.motion;
    let yaw = finite(character.yaw);
    let (sy, cy) = yaw.sin_cos();
    let right = Vec3::new(cy, 0.0, -sy);
    let forward = Vec3::new(-sy, 0.0, -cy);
    let movement = Vec2::new(
        finite(motor.input.movement.x * motor.profile.strafe).clamp(-1.0, 1.0),
        finite(motor.input.movement.y).clamp(-1.0, 1.0),
    )
    .clamp_length_max(1.0);
    let horizontal = (right * movement.x + forward * movement.y) * motor.profile.speed;
    let target = Vec2::new(horizontal.x, horizontal.z);
    let previous = physics::bounded_horizontal(
        Vec2::new(state.velocity.x, state.velocity.z) - state.external_velocity,
    );
    let rate = if target.length_squared() < previous.length_squared() {
        motor.profile.braking
    } else {
        motor.profile.acceleration
    };
    let control = if state.grounded {
        1.0
    } else {
        motor.profile.air_control
    };
    let horizontal = previous
        + (target - previous).clamp_length_max(rate * control * motor.dt)
        + state.external_velocity;
    motor.attempted = horizontal;
    state.velocity.x = horizontal.x;
    state.velocity.z = horizontal.y;
}

/// Admit a grounded jump, then integrate gravity.
fn gravity(motor: &mut Motor, character: &mut CharacterState) {
    let state = &mut character.motion;
    if motor.input.jump && motor.profile.jump_speed > 0.0 && state.grounded {
        state.velocity.y = motor.profile.jump_speed;
        state.grounded = false;
    }
    state.velocity.y = (state.velocity.y - motor.profile.gravity * motor.dt).clamp(-60.0, 60.0);
}

/// Resolve X then Z independently: a blocked axis stops and drops its external
/// momentum while the other keeps sliding.
fn sweep_horizontal(motor: &mut Motor, character: &mut CharacterState) {
    let state = &mut character.motion;
    if physics::sweep_with_bodies(
        motor.world,
        &mut state.position,
        0,
        state.velocity.x * motor.dt,
        motor.bodies,
        None,
        motor.body.shape,
    ) {
        state.velocity.x = 0.0;
        state.external_velocity.x = 0.0;
    }
    if physics::sweep_with_bodies(
        motor.world,
        &mut state.position,
        2,
        state.velocity.z * motor.dt,
        motor.bodies,
        None,
        motor.body.shape,
    ) {
        state.velocity.z = 0.0;
        state.external_velocity.y = 0.0;
    }
}

/// Resolve Y, snapping grounded actors down a full step so descending slopes
/// keep support, then re-probe so walking off an edge clears grounded same-tick.
fn sweep_vertical(motor: &mut Motor, character: &mut CharacterState) {
    let state = &mut character.motion;
    let upward = state.velocity.y > 0.0;
    let snap = state.grounded && state.velocity.y <= 0.0;
    let fall = if snap {
        (state.velocity.y * motor.dt).min(-physics::STEP_HEIGHT)
    } else {
        state.velocity.y * motor.dt
    };
    if physics::sweep_with_bodies(
        motor.world,
        &mut state.position,
        1,
        fall,
        motor.bodies,
        None,
        motor.body.shape,
    ) {
        state.velocity.y = 0.0;
        state.grounded = !upward;
    } else {
        let mut probe = state.position;
        state.grounded = state.velocity.y == 0.0
            && physics::sweep_with_bodies(
                motor.world,
                &mut probe,
                1,
                -0.002,
                motor.bodies,
                None,
                motor.body.shape,
            );
    }
}

/// Exponential drag gives the same momentum decay across different tick sizes.
fn drag(motor: &mut Motor, character: &mut CharacterState) {
    let state = &mut character.motion;
    let drag = if state.grounded {
        motor.profile.ground_drag
    } else {
        motor.profile.air_drag
    };
    let previous = state.external_velocity;
    state.external_velocity *= (-drag * motor.dt).exp();
    let change = state.external_velocity - previous;
    state.velocity.x += change.x;
    state.velocity.z += change.y;
    motor.attempted += change;
}
