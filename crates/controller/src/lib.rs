//! Input-source-independent grounded locomotion. See `docs/controller.md`.
mod motor;
pub mod player;
mod plugin;

use bevy_ecs::prelude::Component;
use glam::{Vec2, Vec3};
pub use motor::step_character;
pub use physics::{CollisionShape, DynamicCollider, FIXED_DT, PlayerState};
pub use player::{PlayerInput, step_player, step_player_with_bodies};
pub use plugin::{ControllerPlugin, ControllerSet, ObservedBodies};

/// Held body-relative axes: +X right, +Y forward (-Z at yaw zero).
/// Turn is a held fraction of the profile's maximum yaw rate; positive turns left.
/// Jump is a one-tick request, consumed even when airborne. No automatic buffering.
/// Attack is a one-tick action edge: the motor ignores it, and the host's combat
/// system consumes and clears it each tick so policies submit it like jump.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct CharacterIntent {
    pub movement: Vec2,
    pub turn: f32,
    pub jump: bool,
    pub attack: bool,
    /// Item the character swings with; the host maps it to a melee spec.
    pub held_item: u32,
    /// Held sprint request: scales target speed by the profile's sprint multiplier.
    pub sprint: bool,
}
impl CharacterIntent {
    pub fn bounded(self) -> Self {
        Self {
            movement: Vec2::new(
                finite(self.movement.x).clamp(-1.0, 1.0),
                finite(self.movement.y).clamp(-1.0, 1.0),
            )
            .clamp_length_max(1.0),
            turn: finite(self.turn).clamp(-1.0, 1.0),
            jump: self.jump,
            attack: self.attack,
            held_item: self.held_item,
            sprint: self.sprint,
        }
    }
}

/// Physical configuration, independent of control source and locomotion tuning.
#[derive(Component, Clone, Copy, Debug)]
pub struct CharacterBody {
    pub shape: CollisionShape,
    mass: f32,
}
impl CharacterBody {
    pub fn new(shape: CollisionShape, mass: f32) -> Option<Self> {
        (mass.is_finite() && (0.01..=100_000.0).contains(&mass)).then_some(Self { shape, mass })
    }
    pub fn mass(self) -> f32 {
        self.mass
    }
}
impl Default for CharacterBody {
    fn default() -> Self {
        Self {
            shape: CollisionShape::default(),
            mass: physics::PLAYER_MASS,
        }
    }
}

/// Ground locomotion tuning in metres, seconds and radians. Invalid values become
/// zero and excessive values are bounded at the motor boundary.
#[derive(Component, Clone, Copy, Debug)]
pub struct MovementProfile {
    pub speed: f32,
    /// Target-speed multiplier while sprinting.
    pub sprint_mult: f32,
    pub acceleration: f32,
    pub braking: f32,
    pub air_control: f32,
    pub strafe: f32,
    pub turn_rate: f32,
    pub gravity: f32,
    pub jump_speed: f32,
    pub ground_drag: f32,
    pub air_drag: f32,
}
impl Default for MovementProfile {
    fn default() -> Self {
        Self {
            speed: 6.0,
            sprint_mult: 1.5,
            acceleration: 10_000.0,
            braking: 10_000.0,
            air_control: 1.0,
            strafe: 1.0,
            turn_rate: std::f32::consts::PI,
            gravity: 24.0,
            jump_speed: 8.0,
            ground_drag: 8.0,
            air_drag: 1.0,
        }
    }
}
impl MovementProfile {
    pub(crate) fn bounded(self) -> Self {
        Self {
            speed: finite(self.speed).clamp(0.0, 60.0),
            sprint_mult: finite(self.sprint_mult).clamp(1.0, 4.0),
            acceleration: finite(self.acceleration).clamp(0.0, 10_000.0),
            braking: finite(self.braking).clamp(0.0, 10_000.0),
            air_control: finite(self.air_control).clamp(0.0, 1.0),
            strafe: finite(self.strafe).clamp(0.0, 1.0),
            turn_rate: finite(self.turn_rate).clamp(0.0, 20.0),
            gravity: finite(self.gravity).clamp(0.0, 100.0),
            jump_speed: finite(self.jump_speed).clamp(0.0, 60.0),
            ground_drag: finite(self.ground_drag).clamp(0.0, 100.0),
            air_drag: finite(self.air_drag).clamp(0.0, 100.0),
        }
    }
}

/// Exclusive locomotion/action mode. Ground and Air are derived from support;
/// Cast, Dash and Blink are timed actions that lock input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Ground,
    Air,
    Cast,
    Dash,
    Blink,
}

/// Status effect kinds. Knockback stays physics-owned; these gate action and
/// movement and are applied and cleared by the host's combat system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    Stun,
    Sleep,
    Root,
    Slow,
    Silence,
    Knockup,
    Taunt,
    Fear,
    Blind,
}

/// One active status effect; `remaining` counts down in fixed ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub kind: StatusKind,
    pub remaining: u16,
}

/// Maximum simultaneously active statuses per character.
pub const MAX_STATUSES: usize = 8;

/// Bounded set of active statuses in application order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatusList {
    entries: [Option<Status>; MAX_STATUSES],
}
impl StatusList {
    /// Active statuses in application order; `None` slots are free.
    pub fn entries(&self) -> &[Option<Status>; MAX_STATUSES] {
        &self.entries
    }
}

/// Per-tick movement and action gates derived from active statuses and mode.
/// Derived, not stored: the motor recomputes it from `CharacterState` each tick.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Constraints {
    pub action_locked: bool,
    pub movement_locked: bool,
    pub cast_locked: bool,
    pub speed_mult: f32,
    pub turn_mult: f32,
}

/// Complete motor state for replay. Physics motion is feet-anchored; yaw is radians.
/// `motion.noclip` is reserved for the human debug adapter, not a policy action.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct CharacterState {
    pub motion: physics::KinematicState,
    pub yaw: f32,
    pub mode: Mode,
    pub statuses: StatusList,
}
impl CharacterState {
    pub fn apply_impulse(&mut self, body: &CharacterBody, impulse: Vec3) {
        physics::apply_impulse(&mut self.motion, impulse, body.mass);
    }
}

pub(crate) fn finite(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

#[cfg(test)]
mod character_tests;
#[cfg(test)]
mod impulse_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
use glam::IVec3;
#[cfg(test)]
use physics::{PLAYER_MASS, PLAYER_RADIUS, apply_player_impulse};
#[cfg(test)]
use voxel_world::{AIR, VoxelWorld};
#[cfg(test)]
const GRAVITY: f32 = 24.0;

#[cfg(test)]
const SPEED: f32 = 6.0;
#[cfg(test)]
const EPSILON: f32 = 0.0001;
#[cfg(test)]
use physics::{look_direction, overlaps_block};
#[cfg(test)]
fn bounds(position: Vec3) -> (Vec3, Vec3) {
    physics::bounds(position, CollisionShape::default())
}
