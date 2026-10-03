//! Human wire-command adapter. Sequence/order belongs to networking, not the motor.
use crate::{
    CharacterBody, CharacterIntent, CharacterState, MovementProfile, finite, step_character,
};
use glam::{IVec3, Vec2, Vec3};
use physics::{DynamicCollider, PlayerState, look_direction};
use serde::{Deserialize, Serialize};
use voxel_world::VoxelWorld;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Default)]
pub struct PlayerInput {
    pub sequence: u64,
    pub movement: [f32; 2],
    pub yaw: f32,
    pub pitch: f32,
    pub jump: bool,
    pub descend: bool,
    /// Desired mode, not a toggle edge, so input replay is idempotent.
    pub noclip: bool,
    /// One-tick melee request; the redundant input tail makes the edge reliable.
    pub attack: bool,
    /// Held item id; the server maps it to melee specs.
    pub selected: u32,
}

#[path = "noclip.rs"]
mod noclip;
const EPSILON: f32 = 0.0001;
fn bounds(position: Vec3) -> (Vec3, Vec3) {
    physics::bounds(position, physics::CollisionShape::default())
}
/// Discrete solids for noclip clearance: placed cubes only. Smooth terrain is
/// checked through `density_at`; unloaded cells stay passable as before.
fn solid(world: &VoxelWorld, cell: IVec3) -> bool {
    world.voxel(cell).is_some_and(|v| v.placed)
}

pub fn step_player(world: &VoxelWorld, state: &mut PlayerState, input: &PlayerInput, dt: f32) {
    step_player_with_bodies(world, state, input, dt, &[]);
}

/// Stateless conversion used by authority, prediction, and reconciliation.
/// `jump` requests one tick on the ground, but is held ascent in debug flight.
/// Returns the horizontal velocity the character attempted this tick, before
/// terrain or loose bodies clamped it; the server feeds it to GPU body push.
pub fn step_player_with_bodies(
    world: &VoxelWorld,
    state: &mut PlayerState,
    input: &PlayerInput,
    dt: f32,
    bodies: &[DynamicCollider],
) -> Vec2 {
    if !dt.is_finite() || dt <= 0.0 {
        return Vec2::new(state.velocity.x, state.velocity.z);
    }
    if !state.position.is_finite() || state.position.abs().max_element() > 32_000_000.0 {
        *state = PlayerState::default();
    }
    if !state.velocity.is_finite() {
        state.velocity = Vec3::ZERO;
    }
    state.external_velocity = physics::bounded_horizontal(state.external_velocity);
    if noclip::step(world, state, input, dt.min(0.25), bodies) {
        return Vec2::new(state.velocity.x, state.velocity.z);
    }
    let mut character = CharacterState {
        motion: *state,
        yaw: finite(input.yaw),
        ..Default::default()
    };
    let mut intent = CharacterIntent {
        movement: Vec2::from_array(input.movement),
        turn: 0.0,
        jump: input.jump,
        attack: input.attack,
        held_item: 0,
        ..Default::default()
    };
    let attempted = step_character(
        world,
        &mut character,
        &CharacterBody::default(),
        &MovementProfile::default(),
        &mut intent,
        dt,
        bodies,
    )
    .attempted;
    *state = character.motion;
    attempted
}
