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
/// Jump is a one-tick request, consumed even when airborne; the motor buffers it
/// and applies coyote time so presses just before landing or just after leaving
/// the ground still fire.
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
    /// Held crouch request: lowers the body and scales target speed.
    pub crouch: bool,
    /// One-tick ability edges, one per slot; the motor consumes and clears them.
    pub ability: [bool; MAX_SLOTS],
    /// Body-relative aim direction for abilities; zero means "facing".
    pub aim: Vec2,
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
            crouch: self.crouch,
            ability: self.ability,
            aim: Vec2::new(
                finite(self.aim.x).clamp(-1.0, 1.0),
                finite(self.aim.y).clamp(-1.0, 1.0),
            )
            .clamp_length_max(1.0),
        }
    }
}

/// Physical configuration, independent of control source and locomotion tuning.
#[derive(Component, Clone, Copy, Debug)]
pub struct CharacterBody {
    pub shape: CollisionShape,
    /// Per-species ability table; empty slots mean the species has no ability.
    pub abilities: AbilityTable,
    mass: f32,
}
impl CharacterBody {
    pub fn new(shape: CollisionShape, mass: f32) -> Option<Self> {
        (mass.is_finite() && (0.01..=100_000.0).contains(&mass)).then_some(Self {
            shape,
            abilities: AbilityTable::default(),
            mass,
        })
    }
    /// Attach an ability table to this body.
    pub fn with_abilities(mut self, abilities: AbilityTable) -> Self {
        self.abilities = abilities;
        self
    }
    pub fn mass(self) -> f32 {
        self.mass
    }
}
impl Default for CharacterBody {
    fn default() -> Self {
        Self {
            shape: CollisionShape::default(),
            abilities: AbilityTable::default(),
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
    /// Ticks after leaving the ground during which a jump still fires.
    pub coyote_ticks: u8,
    /// Ticks a jump request stays buffered while airborne.
    pub jump_buffer_ticks: u8,
    /// Maximum rise the motor auto-steps over a blocked horizontal sweep.
    pub step_height: f32,
    /// Cosine of the steepest walkable slope; steeper ground slides the actor.
    pub max_slope_cos: f32,
    /// Target-speed multiplier while crouching.
    pub crouch_mult: f32,
    /// Body height while crouching, in metres.
    pub crouch_height: f32,
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
            coyote_ticks: 6,
            jump_buffer_ticks: 6,
            step_height: 0.6,
            max_slope_cos: std::f32::consts::FRAC_1_SQRT_2,
            crouch_mult: 0.5,
            crouch_height: 0.9,
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
            coyote_ticks: self.coyote_ticks.min(60),
            jump_buffer_ticks: self.jump_buffer_ticks.min(60),
            step_height: finite(self.step_height).clamp(0.0, 2.0),
            max_slope_cos: finite(self.max_slope_cos).clamp(0.0, 1.0),
            crouch_mult: finite(self.crouch_mult).clamp(0.0, 1.0),
            crouch_height: finite(self.crouch_height).clamp(0.01, 16.0),
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

    /// Apply or refresh a status. Strongest-wins per kind: a longer remaining
    /// duration replaces a shorter one, otherwise the existing entry stands.
    /// A full list drops the new status rather than evicting an active one.
    pub fn apply(&mut self, kind: StatusKind, ticks: u16) {
        for status in self.entries.iter_mut().flatten() {
            if status.kind == kind {
                status.remaining = status.remaining.max(ticks);
                return;
            }
        }
        if let Some(slot) = self.entries.iter_mut().find(|entry| entry.is_none()) {
            *slot = Some(Status {
                kind,
                remaining: ticks,
            });
        }
    }

    /// Decrement every active status and drop the ones that expire.
    pub fn tick(&mut self) {
        for entry in self.entries.iter_mut() {
            if let Some(status) = entry {
                status.remaining = status.remaining.saturating_sub(1);
                if status.remaining == 0 {
                    *entry = None;
                }
            }
        }
    }

    /// Movement and action gates derived from the active statuses. Stun, sleep
    /// and knockup lock everything; root locks movement; silence locks casts;
    /// slow scales speed. Taunt, fear and blind are host-level, not motor gates.
    pub fn constraints(&self) -> Constraints {
        let mut constraints = Constraints {
            speed_mult: 1.0,
            turn_mult: 1.0,
            ..Default::default()
        };
        for status in self.entries.iter().flatten() {
            match status.kind {
                StatusKind::Stun | StatusKind::Sleep | StatusKind::Knockup => {
                    constraints.action_locked = true;
                    constraints.movement_locked = true;
                }
                StatusKind::Root => constraints.movement_locked = true,
                StatusKind::Silence => constraints.cast_locked = true,
                StatusKind::Slow => constraints.speed_mult = constraints.speed_mult.min(0.5),
                StatusKind::Taunt | StatusKind::Fear | StatusKind::Blind => {}
            }
        }
        constraints
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

/// Number of ability slots per character.
pub const MAX_SLOTS: usize = 3;

/// What an ability does when it resolves. The host owns the effect; the motor
/// owns the cast, the cooldown and the movement lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbilityKind {
    /// Burst of movement along the aim that still collides with the world.
    Dash,
    /// Teleport to a clear point along the aim.
    Blink,
    /// Spawns a travelling projectile along the aim.
    Projectile,
    /// Applies an area effect at a point along the aim.
    Area,
}

/// What the caster may do during the cast time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CastBehavior {
    /// Rooted for the cast time: the default telegraph.
    #[default]
    Root,
    /// Slowed for the cast time.
    Slow,
    /// No cast time; the ability resolves on the tick it is requested.
    Instant,
}

/// Authored ability parameters. Data-driven so an `"ability"` package kind can
/// author these later, exactly like `MeleeSpec`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AbilitySpec {
    pub kind: AbilityKind,
    pub behavior: CastBehavior,
    /// Cast time in fixed ticks; zero for instant abilities.
    pub cast_ticks: u16,
    /// Cooldown in fixed ticks, started when the ability resolves.
    pub cooldown_ticks: u16,
    /// Dash speed (m/s) or blink range (m); unused by projectile and area.
    pub magnitude: f32,
}

/// Per-species ability table: one optional spec per slot.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AbilityTable {
    slots: [Option<AbilitySpec>; MAX_SLOTS],
}
impl AbilityTable {
    /// Build a table from per-slot specs; `None` leaves a slot empty.
    pub fn new(slots: [Option<AbilitySpec>; MAX_SLOTS]) -> Self {
        Self { slots }
    }
    /// The spec in `slot`, if the species has one.
    pub fn get(&self, slot: usize) -> Option<AbilitySpec> {
        self.slots.get(slot).copied().flatten()
    }
}

/// In-progress cast: which slot, ticks remaining, and the aim captured at start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cast {
    pub slot: u8,
    pub remaining: u16,
    pub aim: Vec2,
}

/// In-progress dash or blink.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dash {
    pub slot: u8,
    pub remaining: u16,
    pub aim: Vec2,
    pub speed: f32,
}

/// Per-tick motor output: the horizontal velocity the character attempted, and
/// the ability slots that resolved this tick for the host to apply effects to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MotorOutput {
    pub attempted: Vec2,
    pub fired: [bool; MAX_SLOTS],
}

/// Complete motor state for replay. Physics motion is feet-anchored; yaw is radians.
/// `motion.noclip` is reserved for the human debug adapter, not a policy action.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct CharacterState {
    pub motion: physics::KinematicState,
    pub yaw: f32,
    pub mode: Mode,
    pub statuses: StatusList,
    /// Ticks of coyote time remaining after leaving the ground.
    pub coyote: u8,
    /// Ticks a buffered jump request stays live.
    pub jump_buffer: u8,
    /// Whether the body is currently lowered by a crouch.
    pub crouching: bool,
    /// Remaining cooldown ticks per ability slot.
    pub cooldowns: [u16; MAX_SLOTS],
    /// In-progress cast, if any.
    pub cast: Option<Cast>,
    /// In-progress dash or blink, if any.
    pub dash: Option<Dash>,
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
