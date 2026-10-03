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
    /// Ground normal from the support probe; `Vec3::Y` when airborne.
    support: Vec3,
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
        support: Vec3::Y,
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
    let grounded = {
        let state = &mut character.motion;
        physics::separate_bodies(motor.world, state, motor.bodies, motor.body.shape);
        let mut probe = state.position;
        state.velocity.y <= 0.0
            && physics::sweep_with_bodies(
                motor.world,
                &mut probe,
                1,
                -0.002,
                motor.bodies,
                None,
                motor.body.shape,
            )
    };
    character.motion.grounded = grounded;
    // Ground normal drives the slope limit; airborne actors report flat.
    motor.support = if grounded {
        physics::support(
            motor.world,
            character.motion.position,
            motor.body.shape,
            motor.bodies,
        )
        .map(|support| support.normal)
        .unwrap_or(Vec3::Y)
    } else {
        Vec3::Y
    };
    // Coyote time: keep a jump available briefly after walking off an edge.
    character.coyote = if grounded {
        motor.profile.coyote_ticks
    } else {
        character.coyote.saturating_sub(1)
    };
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
    let speed = motor.profile.speed
        * if motor.input.sprint {
            motor.profile.sprint_mult
        } else {
            1.0
        };
    let horizontal = (right * movement.x + forward * movement.y) * speed;
    let target = Vec2::new(horizontal.x, horizontal.z);
    let previous = physics::bounded_horizontal(
        Vec2::new(state.velocity.x, state.velocity.z) - state.external_velocity,
    );
    // Ground steeper than the walkable limit overrides control: the actor
    // slides downhill, accumulating up to the profile speed.
    if state.grounded && motor.support.y < motor.profile.max_slope_cos {
        let downhill = (motor.support * motor.support.y - Vec3::Y).normalize_or_zero();
        let slide = Vec2::new(downhill.x, downhill.z) * motor.profile.gravity * motor.dt;
        let slid = (previous + slide).clamp_length_max(motor.profile.speed) + state.external_velocity;
        motor.attempted = slid;
        state.velocity.x = slid.x;
        state.velocity.z = slid.y;
        return;
    }
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
    // Buffer the press so a jump requested just before landing still fires.
    character.jump_buffer = if motor.input.jump {
        motor.profile.jump_buffer_ticks
    } else {
        character.jump_buffer.saturating_sub(1)
    };
    let state = &mut character.motion;
    if character.jump_buffer > 0
        && (state.grounded || character.coyote > 0)
        && motor.profile.jump_speed > 0.0
    {
        state.velocity.y = motor.profile.jump_speed;
        state.grounded = false;
        character.jump_buffer = 0;
        character.coyote = 0;
    }
    state.velocity.y = (state.velocity.y - motor.profile.gravity * motor.dt).clamp(-60.0, 60.0);
}

/// Resolve X then Z independently: a blocked axis stops and drops its external
/// momentum while the other keeps sliding. A grounded actor first tries to step
/// over a low obstruction instead of stopping.
fn sweep_horizontal(motor: &mut Motor, character: &mut CharacterState) {
    let state = &mut character.motion;
    let dx = state.velocity.x * motor.dt;
    if physics::sweep_with_bodies(
        motor.world,
        &mut state.position,
        0,
        dx,
        motor.bodies,
        None,
        motor.body.shape,
    ) && !try_step(motor, state, 0, dx)
    {
        state.velocity.x = 0.0;
        state.external_velocity.x = 0.0;
    }
    let dz = state.velocity.z * motor.dt;
    if physics::sweep_with_bodies(
        motor.world,
        &mut state.position,
        2,
        dz,
        motor.bodies,
        None,
        motor.body.shape,
    ) && !try_step(motor, state, 2, dz)
    {
        state.velocity.z = 0.0;
        state.external_velocity.y = 0.0;
    }
}

/// Step over a low obstruction: lift by the profile's step height, re-sweep the
/// blocked axis, then settle back down. Only grounded actors step, and the
/// lifted pose must be clear so a ceiling still blocks.
fn try_step(
    motor: &mut Motor,
    state: &mut physics::KinematicState,
    axis: usize,
    distance: f32,
) -> bool {
    if !state.grounded || motor.profile.step_height <= 0.0 {
        return false;
    }
    let lifted = state.position + Vec3::Y * motor.profile.step_height;
    if !physics::clearance(motor.world, lifted, motor.body.shape, motor.bodies) {
        return false;
    }
    let mut probe = lifted;
    physics::sweep_with_bodies(
        motor.world,
        &mut probe,
        axis,
        distance,
        motor.bodies,
        None,
        motor.body.shape,
    );
    if (probe[axis] - lifted[axis]).abs() < 1e-4 {
        return false;
    }
    physics::sweep_with_bodies(
        motor.world,
        &mut probe,
        1,
        -motor.profile.step_height,
        motor.bodies,
        None,
        motor.body.shape,
    );
    state.position = probe;
    true
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
