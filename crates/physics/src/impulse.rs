use super::{PLAYER_MASS, PlayerState, finite};
use glam::{Vec2, Vec3};

const MAX_EXTERNAL_SPEED: f32 = 60.0;

/// Apply an impulse in N*s. Horizontal momentum persists alongside movement input;
/// vertical momentum follows gravity. Noclip players ignore physical impulses.
/// External horizontal speed and vertical speed are each limited to 60 m/s.
pub fn apply_player_impulse(state: &mut PlayerState, impulse: Vec3) {
    apply_impulse(state, impulse, PLAYER_MASS);
}

/// Apply a finite physical impulse to a kinematic actor of the given mass (kg).
pub fn apply_impulse(state: &mut PlayerState, impulse: Vec3, mass: f32) {
    if state.noclip || !impulse.is_finite() || !mass.is_finite() || mass <= 0.0 {
        return;
    }
    state.external_velocity = bounded_horizontal(state.external_velocity);
    if !state.velocity.is_finite() {
        state.velocity = Vec3::ZERO;
    }
    let delta = impulse / mass;
    let previous = state.external_velocity;
    state.external_velocity = bounded_horizontal(previous + Vec2::new(delta.x, delta.z));
    // Only apply the admitted change, so repeated blasts cannot bypass the cap.
    let change = state.external_velocity - previous;
    state.velocity.x += change.x;
    state.velocity.z += change.y;
    state.velocity.y =
        (finite(state.velocity.y) + delta.y).clamp(-MAX_EXTERNAL_SPEED, MAX_EXTERNAL_SPEED);
    if delta.y > 0.0 && state.velocity.y > 0.0 {
        state.grounded = false;
    }
}

pub fn bounded_horizontal(velocity: Vec2) -> Vec2 {
    if !velocity.is_finite() {
        return Vec2::ZERO;
    }
    // Compute the length in f64 to handle even finite f32::MAX impulses safely.
    velocity
        .as_dvec2()
        .clamp_length_max(f64::from(MAX_EXTERNAL_SPEED))
        .as_vec2()
}

pub(super) fn horizontal_component(axis: usize) -> Option<usize> {
    match axis {
        0 => Some(0),
        2 => Some(1),
        _ => None,
    }
}
