//! Human wire-command adapter. Sequence/order belongs to networking, not the motor.
use crate::{
    CharacterBody, CharacterIntent, CharacterState, MovementProfile, finite, step_character,
};
use glam::{IVec3, Vec2, Vec3};
use physics::{DynamicCollider, PlayerState, look_direction};
use serde::{Deserialize, Serialize};
use voxel_world::{AIR, VoxelWorld};

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
}

#[path = "noclip.rs"]
mod noclip;
const EPSILON: f32 = 0.0001;
fn bounds(position: Vec3) -> (Vec3, Vec3) {
    physics::bounds(position, physics::CollisionShape::default())
}
fn solid(world: &VoxelWorld, cell: IVec3) -> bool {
    world.block(cell) != Some(AIR)
}

pub fn step_player(world: &VoxelWorld, state: &mut PlayerState, input: &PlayerInput, dt: f32) {
    step_player_with_bodies(world, state, input, dt, &[]);
}

/// Stateless conversion used by authority, prediction, and reconciliation.
/// `jump` requests one tick on the ground, but is held ascent in debug flight.
pub fn step_player_with_bodies(
    world: &VoxelWorld,
    state: &mut PlayerState,
    input: &PlayerInput,
    dt: f32,
    bodies: &[DynamicCollider],
) {
    if !dt.is_finite() || dt <= 0.0 {
        return;
    }
    if !state.position.is_finite() || state.position.abs().max_element() > 32_000_000.0 {
        *state = PlayerState::default();
    }
    if !state.velocity.is_finite() {
        state.velocity = Vec3::ZERO;
    }
    state.external_velocity = physics::bounded_horizontal(state.external_velocity);
    if noclip::step(world, state, input, dt.min(0.25), bodies) {
        return;
    }
    let mut character = CharacterState {
        motion: *state,
        yaw: finite(input.yaw),
    };
    let mut intent = CharacterIntent {
        movement: Vec2::from_array(input.movement),
        turn: 0.0,
        jump: input.jump,
    };
    step_character(
        world,
        &mut character,
        &CharacterBody::default(),
        &MovementProfile::default(),
        &mut intent,
        dt,
        bodies,
    );
    *state = character.motion;
}
