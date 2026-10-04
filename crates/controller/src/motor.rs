use crate::{
    AbilityKind, BasicAttackKind, Cast, CastBehavior, CharacterBody, CharacterIntent,
    CharacterState, CollisionShape, Constraints, Dash, MAX_SLOTS, Mode, MotorOutput,
    MovementProfile, SpeedModifiers, Transition, Transitions, finite,
};
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
    /// Effective collision shape for this tick; a crouch lowers the body.
    shape: CollisionShape,
    /// Movement and action gates derived from the active statuses.
    constraints: Constraints,
    /// Ability slots that resolved this tick, with their aim, for the host.
    fired: [Option<Vec2>; MAX_SLOTS],
    /// Whether the held weapon's basic attack fired this tick.
    basic_attack: bool,
    /// Draw fraction reached this tick, for the host and the HUD.
    charge: f32,
    /// Release edge with the fraction reached, when the draw cleared this tick.
    charge_release: Option<f32>,
    /// Movement modifiers applied this tick, in pipeline order.
    speed: SpeedModifiers,
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
) -> MotorOutput {
    if !dt.is_finite() || dt <= 0.0 {
        return MotorOutput {
            attempted: Vec2::new(character.motion.velocity.x, character.motion.velocity.z),
            fired: [None; MAX_SLOTS],
            basic_attack: false,
            charge: draw_fraction(
                character.charge,
                body.basic_attack.map_or(0, |attack| attack.charge_ticks).max(1),
            ),
            charge_release: None,
            speed: SpeedModifiers::default(),
        };
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
        shape: body.shape,
        constraints: Constraints {
            speed_mult: 1.0,
            turn_mult: 1.0,
            ..Default::default()
        },
        attempted: Vec2::ZERO,
        fired: [None; MAX_SLOTS],
        basic_attack: false,
        charge: 0.0,
        charge_release: None,
        speed: SpeedModifiers::default(),
    };
    status(&mut motor, character);
    sanitize(&mut motor, character);
    stance(&mut motor, character);
    contact(&mut motor, character);
    abilities(&mut motor, character);
    charge(&mut motor, character);
    steer(&mut motor, character);
    gravity(&mut motor, character);
    sweep_horizontal(&mut motor, character);
    sweep_vertical(&mut motor, character);
    drag(&mut motor, character);
    MotorOutput {
        attempted: motor.attempted,
        fired: motor.fired,
        basic_attack: motor.basic_attack,
        charge: motor.charge,
        charge_release: motor.charge_release,
        speed: motor.speed,
    }
}

/// Advance status timers and derive this tick's movement and action gates.
fn status(motor: &mut Motor, character: &mut CharacterState) {
    character.statuses.tick();
    motor.constraints = character.statuses.constraints();
}

/// Bound the tick and the replay state, and advance yaw from held turn.
fn sanitize(motor: &mut Motor, character: &mut CharacterState) {
    let state = &mut character.motion;
    character.yaw = (finite(character.yaw)
        + motor.input.turn * motor.profile.turn_rate * motor.dt * motor.constraints.turn_mult)
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

/// Update the crouch stance and the effective collision shape. Standing up
/// requires clearance for the full-height body, so a low ceiling keeps the
/// actor crouched until it can fit.
fn stance(motor: &mut Motor, character: &mut CharacterState) {
    let full = motor.body.shape;
    if motor.input.crouch {
        character.crouching = true;
    } else if character.crouching
        && physics::clearance(motor.world, character.motion.position, full, motor.bodies)
    {
        character.crouching = false;
    }
    motor.shape = if character.crouching {
        CollisionShape::new(
            full.half_width(),
            full.half_depth(),
            motor.profile.crouch_height.min(full.height()),
        )
        .unwrap_or(full)
    } else {
        full
    };
}

/// Repair snapshot overlap, then refresh support with a shallow downward probe.
fn contact(motor: &mut Motor, character: &mut CharacterState) {
    let grounded = {
        let state = &mut character.motion;
        physics::separate_bodies(motor.world, state, motor.bodies, motor.shape);
        let mut probe = state.position;
        state.velocity.y <= 0.0
            && physics::sweep_with_bodies(
                motor.world,
                &mut probe,
                1,
                -0.002,
                motor.bodies,
                None,
                motor.shape,
            )
    };
    character.motion.grounded = grounded;
    // Ground normal drives the slope limit; airborne actors report flat.
    motor.support = if grounded {
        physics::support(
            motor.world,
            character.motion.position,
            motor.shape,
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

/// Fixed duration of a dash in ticks.
const DASH_TICKS: u16 = 12;
/// Step size when searching for a clear blink destination.
const BLINK_STEP: f32 = 0.5;

/// Pure transition function: intent + state + body + constraints -> the
/// transitions to apply this tick. No side effects, so it is testable in
/// isolation and replays exactly.
pub fn resolve(
    intent: &CharacterIntent,
    state: &CharacterState,
    body: &CharacterBody,
    constraints: &Constraints,
) -> Transitions {
    let mut transitions = Transitions::default();
    // Basic attack: `attack` is held. A melee or admin weapon fires once on the
    // press edge; a ranged weapon draws while held and fires on release at full
    // charge, cancelling an early release.
    let pressed = intent.attack && !state.attack_held;
    let released = !intent.attack && state.attack_held;
    if let Some(attack) = body.basic_attack {
        match attack.kind {
            BasicAttackKind::Melee | BasicAttackKind::Admin => {
                if pressed && state.basic_attack_cooldown == 0 && !constraints.action_locked {
                    transitions.push(Transition::Swing);
                }
            }
            BasicAttackKind::Ranged => {
                let full = attack.charge_ticks.max(1);
                if intent.attack && !constraints.action_locked {
                    transitions.push(Transition::Draw);
                } else if released && state.drawing {
                    if state.charge >= full && state.basic_attack_cooldown == 0 {
                        transitions.push(Transition::Fire {
                            charge: state.charge,
                        });
                    } else {
                        transitions.push(Transition::CancelDraw);
                    }
                }
            }
        }
    }
    // A hard status interrupts an in-progress cast or dash.
    if constraints.action_locked {
        if state.cast.is_some() {
            transitions.push(Transition::InterruptCast);
        }
        if state.dash.is_some() {
            transitions.push(Transition::InterruptDash);
        }
    }
    // Timed actions complete on their last tick.
    if let Some(cast) = state.cast
        && cast.remaining <= 1
    {
        transitions.push(Transition::CompleteCast {
            slot: cast.slot,
            aim: cast.aim,
        });
    }
    if let Some(dash) = state.dash
        && dash.remaining <= 1
    {
        transitions.push(Transition::EndDash);
    }
    // A new ability only starts when nothing else is in progress.
    if state.cast.is_none() && state.dash.is_none() && !constraints.cast_locked {
        for slot in 0..MAX_SLOTS {
            if !intent.ability[slot] || state.cooldowns[slot] > 0 {
                continue;
            }
            let Some(spec) = body.abilities.get(slot) else {
                continue;
            };
            let aim = aim_vector(intent, state);
            if spec.behavior == CastBehavior::Instant {
                transitions.push(Transition::FireAbility {
                    slot: slot as u8,
                    aim,
                });
            } else {
                transitions.push(Transition::BeginCast {
                    slot: slot as u8,
                    aim,
                });
            }
            break;
        }
    }
    transitions
}

/// Apply one transition. Deterministic; the only place transitions mutate state.
fn apply(transition: Transition, motor: &mut Motor, character: &mut CharacterState) {
    match transition {
        Transition::Swing => {
            if let Some(attack) = motor.body.basic_attack {
                character.basic_attack_cooldown = attack.cooldown_ticks;
            }
            motor.basic_attack = true;
        }
        Transition::Draw => {
            character.drawing = true;
            let full = motor.draw_ticks();
            character.charge = character.charge.saturating_add(1).min(full);
        }
        Transition::Fire { charge } => {
            if let Some(attack) = motor.body.basic_attack {
                character.basic_attack_cooldown = attack.cooldown_ticks;
            }
            character.drawing = false;
            character.charge = 0;
            motor.charge_release = Some(draw_fraction(charge, motor.draw_ticks()));
        }
        Transition::CancelDraw => {
            character.drawing = false;
            character.charge = 0;
        }
        Transition::FireAbility { slot, aim } => resolve_ability(motor, character, slot, aim),
        Transition::BeginCast { slot, aim } => {
            let Some(spec) = motor.body.abilities.get(slot as usize) else {
                return;
            };
            character.cast = Some(Cast {
                slot,
                remaining: spec.cast_ticks.max(1),
                aim,
            });
        }
        Transition::CompleteCast { slot, aim } => {
            character.cast = None;
            resolve_ability(motor, character, slot, aim);
        }
        Transition::InterruptCast => character.cast = None,
        Transition::InterruptDash => character.dash = None,
        Transition::EndDash => character.dash = None,
    }
}

/// Consume ability edges, advance casts and dashes, and tick cooldowns. The
/// decision is `resolve` (pure); this stage only applies it and advances timers.
fn abilities(motor: &mut Motor, character: &mut CharacterState) {
    for cooldown in character.cooldowns.iter_mut() {
        *cooldown = cooldown.saturating_sub(1);
    }
    character.basic_attack_cooldown = character.basic_attack_cooldown.saturating_sub(1);
    let was_casting = character.cast.is_some();
    let was_dashing = character.dash.is_some();
    let transitions = resolve(&motor.input, character, motor.body, &motor.constraints);
    character.attack_held = motor.input.attack;
    for transition in transitions.iter() {
        apply(transition, motor, character);
    }
    // Advance only actions already in progress: one begun this tick starts its
    // countdown next tick, so a cast of N ticks resolves on the Nth tick.
    if was_casting
        && let Some(cast) = character.cast
    {
        character.cast = Some(Cast {
            remaining: cast.remaining.saturating_sub(1),
            ..cast
        });
    }
    if was_dashing
        && let Some(dash) = character.dash
    {
        character.dash = Some(Dash {
            remaining: dash.remaining.saturating_sub(1),
            ..dash
        });
    }
    // Derive the observation mode from the support and the timed actions.
    character.mode = if character.cast.is_some() {
        Mode::Cast
    } else if character.dash.is_some() {
        Mode::Dash
    } else if character.motion.grounded {
        Mode::Ground
    } else {
        Mode::Air
    };
}

/// Resolve an ability slot: mark it fired, start its cooldown, and begin any
/// movement.
fn resolve_ability(motor: &mut Motor, character: &mut CharacterState, slot: u8, aim: Vec2) {
    let Some(spec) = motor.body.abilities.get(slot as usize) else {
        return;
    };
    motor.fired[slot as usize] = Some(aim);
    character.cooldowns[slot as usize] = spec.cooldown_ticks;
    match spec.kind {
        AbilityKind::Dash => {
            character.dash = Some(Dash {
                slot,
                remaining: DASH_TICKS,
                aim,
                speed: spec.magnitude,
            });
        }
        AbilityKind::Blink => {
            let mut best = character.motion.position;
            let mut step = BLINK_STEP;
            while step <= spec.magnitude {
                let candidate = character.motion.position + Vec3::new(aim.x, 0.0, aim.y) * step;
                if !physics::clearance(motor.world, candidate, motor.shape, motor.bodies) {
                    break;
                }
                best = candidate;
                step += BLINK_STEP;
            }
            character.motion.position = best;
        }
        AbilityKind::Projectile | AbilityKind::Area => {}
    }
}

/// Report the current draw fraction for the movement slow and the HUD. The
/// `abilities` stage owns advancing and releasing the draw.
fn charge(motor: &mut Motor, character: &mut CharacterState) {
    motor.charge = if character.drawing {
        draw_fraction(character.charge, motor.draw_ticks())
    } else {
        0.0
    };
}

/// Draw fraction in 0..1; a zero full draw reads as fully charged.
fn draw_fraction(charge: u16, full: u16) -> f32 {
    if full == 0 {
        1.0
    } else {
        (f32::from(charge) / f32::from(full)).clamp(0.0, 1.0)
    }
}

/// Target-speed multiplier for a draw fraction: 1.0 undrawn, `mult` at full.
fn draw_speed(mult: f32, fraction: f32) -> f32 {
    1.0 + (mult - 1.0) * fraction
}

impl Motor<'_> {
    /// Ticks a ranged weapon draws before firing; 1 for any other weapon, so an
    /// uncharged attack never reports a draw fraction.
    fn draw_ticks(&self) -> u16 {
        self.body
            .basic_attack
            .filter(|attack| attack.kind == BasicAttackKind::Ranged)
            .map_or(1, |attack| attack.charge_ticks.max(1))
    }
}

/// World-space aim direction: the body-relative aim rotated by yaw, or forward
/// when the aim is zero. Pure, so `resolve` can call it.
fn aim_vector(intent: &CharacterIntent, state: &CharacterState) -> Vec2 {
    let yaw = finite(state.yaw);
    let (sy, cy) = yaw.sin_cos();
    let right = Vec2::new(cy, -sy);
    let forward = Vec2::new(-sy, -cy);
    let world = right * intent.aim.x + forward * intent.aim.y;
    if world.length_squared() > 1e-6 {
        world.normalize()
    } else {
        forward
    }
}

/// Turn held body-relative input into a target horizontal velocity and
/// accelerate or brake toward it, keeping external momentum separate.
fn steer(motor: &mut Motor, character: &mut CharacterState) {
    let state = &mut character.motion;
    // A dash overrides input with its own velocity; a root cast holds position.
    if let Some(dash) = character.dash {
        let velocity = dash.aim * dash.speed;
        motor.attempted = velocity;
        state.velocity.x = velocity.x;
        state.velocity.z = velocity.y;
        return;
    }
    if character.cast.is_some_and(|cast| {
        motor
            .body
            .abilities
            .get(cast.slot as usize)
            .is_some_and(|spec| spec.behavior == CastBehavior::Root)
    }) {
        motor.attempted = state.external_velocity;
        state.velocity.x = state.external_velocity.x;
        state.velocity.z = state.external_velocity.y;
        return;
    }
    let yaw = finite(character.yaw);
    let (sy, cy) = yaw.sin_cos();
    let right = Vec3::new(cy, 0.0, -sy);
    let forward = Vec3::new(-sy, 0.0, -cy);
    let movement = if motor.constraints.movement_locked {
        Vec2::ZERO
    } else {
        Vec2::new(
            finite(motor.input.movement.x * motor.profile.strafe).clamp(-1.0, 1.0),
            finite(motor.input.movement.y).clamp(-1.0, 1.0),
        )
        .clamp_length_max(1.0)
    };
    let cast_slow = character.cast.is_some_and(|cast| {
        motor
            .body
            .abilities
            .get(cast.slot as usize)
            .is_some_and(|spec| spec.behavior == CastBehavior::Slow)
    });
    // Movement modifiers, in pipeline order. Each is multiplicative on the
    // target speed; external momentum stays separate and unscaled.
    motor.speed = SpeedModifiers {
        status: motor.constraints.speed_mult,
        stance: if character.crouching {
            motor.profile.crouch_mult
        } else {
            1.0
        },
        sprint: if motor.input.sprint {
            motor.profile.sprint_mult
        } else {
            1.0
        },
        cast: if cast_slow { 0.5 } else { 1.0 },
        charge: draw_speed(motor.profile.charge_mult, motor.charge),
    };
    let speed = motor.profile.speed * motor.speed.factor();
    let horizontal = (right * movement.x + forward * movement.y) * speed;
    let target = Vec2::new(horizontal.x, horizontal.z);
    let previous = physics::bounded_horizontal(
        Vec2::new(state.velocity.x, state.velocity.z) - state.external_velocity,
    );
    // Ground steeper than the walkable limit overrides control: the actor
    // slides downhill, accumulating up to the profile speed.
    if state.grounded
        && !motor.constraints.movement_locked
        && motor.support.y < motor.profile.max_slope_cos
    {
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
        && !motor.constraints.action_locked
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
        motor.shape,
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
        motor.shape,
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
    if !physics::clearance(motor.world, lifted, motor.shape, motor.bodies) {
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
        motor.shape,
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
        motor.shape,
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
        motor.shape,
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
                motor.shape,
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
