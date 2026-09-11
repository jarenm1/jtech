mod impulse;
use bevy_app::{App, FixedUpdate, Plugin};
use bevy_ecs::schedule::{IntoScheduleConfigs, SystemSet};
use glam::{IVec3, Vec2, Vec3};
pub use impulse::{apply_impulse, apply_player_impulse, bounded_horizontal};
use serde::{Deserialize, Serialize};
use voxel_world::{AIR, VoxelWorld};

pub const PLAYER_RADIUS: f32 = 0.3;
pub const PLAYER_HEIGHT: f32 = 1.8;
/// Player mass in kilograms, used to convert impulses into velocity changes.
pub const PLAYER_MASS: f32 = 80.0;
pub const EYE_HEIGHT: f32 = 1.6;
pub const FIXED_DT: f32 = 1.0 / 60.0;
const EPSILON: f32 = 0.0001;

/// Latest observed unit cube, centered at `position`; callers own snapshot timing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DynamicCollider {
    pub id: u32,
    pub position: Vec3,
    pub velocity: Vec3,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct KinematicState {
    pub position: Vec3,
    pub velocity: Vec3,
    /// Horizontal momentum from external impulses, separate from movement input.
    pub external_velocity: Vec2,
    pub grounded: bool,
    pub noclip: bool,
}
impl Default for KinematicState {
    fn default() -> Self {
        Self {
            position: Vec3::new(0.5, 24.0, 0.5),
            velocity: Vec3::ZERO,
            external_velocity: Vec2::ZERO,
            grounded: false,
            noclip: false,
        }
    }
}
/// Hosts may place input ingestion, shared movement, and replication in these ordered sets.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhysicsSet {
    Input,
    Movement,
    Output,
}
pub struct PhysicsPlugin;
impl Plugin for PhysicsPlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            FixedUpdate,
            (PhysicsSet::Input, PhysicsSet::Movement, PhysicsSet::Output).chain(),
        );
    }
}
fn finite(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}
pub fn look_direction(yaw: f32, pitch: f32) -> Vec3 {
    let yaw = finite(yaw);
    let pitch = finite(pitch).clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2);
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    Vec3::new(-sy * cp, sp, -cy * cp)
}
pub fn bounds(position: Vec3, shape: CollisionShape) -> (Vec3, Vec3) {
    (
        position - Vec3::new(shape.half_width(), 0.0, shape.half_depth()),
        position + Vec3::new(shape.half_width(), shape.height(), shape.half_depth()),
    )
}
pub fn overlaps_block(state: &PlayerState, block: IVec3) -> bool {
    overlaps_shape(state, block, CollisionShape::default())
}

pub fn overlaps_shape(state: &KinematicState, block: IVec3, shape: CollisionShape) -> bool {
    if state.noclip {
        return false;
    }
    let (min, max) = bounds(state.position, shape);
    let block_min = block.as_vec3();
    min.cmplt(block_min + Vec3::ONE).all() && max.cmpgt(block_min).all()
}
fn solid(world: &VoxelWorld, cell: IVec3) -> bool {
    world.block(cell) != Some(AIR)
}

/// Sweeps the leading AABB face across every crossed voxel plane, not just its endpoint.
/// Sequential axes preserve tangential displacement when a wall blocks one component.
fn sweep_axis(
    world: &VoxelWorld,
    position: &mut Vec3,
    axis: usize,
    distance: f32,
    shape: CollisionShape,
) -> bool {
    if distance == 0.0 {
        return false;
    }
    let (min, max) = bounds(*position, shape);
    let positive = distance > 0.0;
    let face = if positive { max[axis] } else { min[axis] };
    let other_a = (axis + 1) % 3;
    let other_b = (axis + 2) % 3;
    let a_min = (min[other_a] + EPSILON).floor() as i32;
    let a_max = (max[other_a] - EPSILON).floor() as i32;
    let b_min = (min[other_b] + EPSILON).floor() as i32;
    let b_max = (max[other_b] - EPSILON).floor() as i32;
    // Include a plane touching the current face so resting contact blocks gravity.
    let first = if positive {
        (face - EPSILON).ceil() as i32
    } else {
        (face + EPSILON).floor() as i32
    };
    let last = if positive {
        (face + distance).floor() as i32
    } else {
        (face + distance).ceil() as i32
    };
    let mut plane = first;
    while if positive {
        plane <= last
    } else {
        plane >= last
    } {
        let mut cell = IVec3::ZERO;
        cell[axis] = if positive { plane } else { plane - 1 };
        for a in a_min..=a_max {
            for b in b_min..=b_max {
                cell[other_a] = a;
                cell[other_b] = b;
                if solid(world, cell) {
                    let allowed = plane as f32 - face;
                    position[axis] += if positive {
                        allowed.max(0.0).min(distance)
                    } else {
                        allowed.min(0.0).max(distance)
                    };
                    return true;
                }
            }
        }
        plane += if positive { 1 } else { -1 };
    }
    position[axis] += distance;
    false
}

/// Sweep against terrain and observed loose cubes without extrapolating snapshots.
pub fn sweep_with_bodies(
    world: &VoxelWorld,
    position: &mut Vec3,
    axis: usize,
    distance: f32,
    bodies: &[DynamicCollider],
    ignore: Option<usize>,
    shape: CollisionShape,
) -> bool {
    if distance == 0.0 {
        return false;
    }
    let start = *position;
    let mut hit = sweep_axis(world, position, axis, distance, shape);
    let mut allowed = position[axis] - start[axis];
    let (min, max) = bounds(start, shape);
    let a = (axis + 1) % 3;
    let b = (axis + 2) % 3;
    for (index, body) in bodies.iter().enumerate() {
        if ignore == Some(index) || !body.position.is_finite() {
            continue;
        }
        let lo = body.position - Vec3::splat(0.5);
        let hi = body.position + Vec3::splat(0.5);
        if max[a] <= lo[a] + EPSILON
            || min[a] >= hi[a] - EPSILON
            || max[b] <= lo[b] + EPSILON
            || min[b] >= hi[b] - EPSILON
        {
            continue;
        }
        let gap = if distance > 0.0 {
            lo[axis] - max[axis]
        } else {
            hi[axis] - min[axis]
        };
        if distance > 0.0 && gap >= -EPSILON && gap <= allowed {
            allowed = gap.max(0.0);
            hit = true;
        } else if distance < 0.0 && gap <= EPSILON && gap >= allowed {
            allowed = gap.min(0.0);
            hit = true;
        }
    }
    position[axis] = start[axis] + allowed;
    hit
}

/// Correct snapshot overlap using bounded, terrain-swept translations. If crushed
/// with no clear escape, tolerate body overlap rather than crossing solid terrain.
pub fn separate_bodies(
    world: &VoxelWorld,
    state: &mut PlayerState,
    bodies: &[DynamicCollider],
    shape: CollisionShape,
) {
    // Keep correction bounded while allowing a larger actor to escape a cube
    // embedded inside it. Shape dimensions are validated and at most 16m.
    let mut budget = 1.0_f32.max(
        shape
            .half_width()
            .max(shape.half_depth())
            .max(shape.height())
            + 0.5,
    );
    for _ in 0..4 {
        let mut corrected = false;
        for (index, body) in bodies.iter().enumerate() {
            if !body.position.is_finite() {
                continue;
            }
            let (min, max) = bounds(state.position, shape);
            let lo = body.position - Vec3::splat(0.5);
            let hi = body.position + Vec3::splat(0.5);
            if !(min.cmplt(hi - Vec3::splat(EPSILON)).all()
                && max.cmpgt(lo + Vec3::splat(EPSILON)).all())
            {
                continue;
            }
            let mut best: Option<(f32, usize, f32, Vec3)> = None;
            for axis in 0..3 {
                for distance in [lo[axis] - max[axis], hi[axis] - min[axis]] {
                    if distance.abs() > budget {
                        continue;
                    }
                    let mut candidate = state.position;
                    sweep_with_bodies(
                        world,
                        &mut candidate,
                        axis,
                        distance,
                        bodies,
                        Some(index),
                        shape,
                    );
                    if (candidate[axis] - state.position[axis] - distance).abs() > EPSILON {
                        continue;
                    }
                    // Near ties favor displacement in the body's travel direction.
                    let opposing = finite(body.velocity[axis]) * distance < 0.0;
                    let score = distance.abs() + if opposing { 0.25 } else { 0.0 };
                    if best.is_none_or(|previous| score < previous.0) {
                        best = Some((score, axis, distance, candidate));
                    }
                }
            }
            if let Some((_, axis, distance, candidate)) = best {
                state.position = candidate;
                budget -= distance.abs();
                if state.velocity[axis] * distance < 0.0 {
                    state.velocity[axis] = 0.0;
                }
                if let Some(component) = impulse::horizontal_component(axis)
                    && state.external_velocity[component] * distance < 0.0
                {
                    state.external_velocity[component] = 0.0;
                }
                corrected = true;
            }
        }
        if !corrected || budget <= EPSILON {
            break;
        }
    }
}

/// Compatibility name for the human network state.
pub type PlayerState = KinematicState;

/// Axis-aligned collider anchored at the center of its feet. Does not rotate with yaw.
/// Dimensions are bounded to keep terrain queries finite and local.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CollisionShape {
    half_width: f32,
    half_depth: f32,
    height: f32,
}
impl CollisionShape {
    pub fn new(half_width: f32, half_depth: f32, height: f32) -> Option<Self> {
        if [half_width, half_depth, height]
            .iter()
            .all(|v| v.is_finite() && (0.01..=16.0).contains(v))
        {
            Some(Self {
                half_width,
                half_depth,
                height,
            })
        } else {
            None
        }
    }
    pub fn half_width(self) -> f32 {
        self.half_width
    }
    pub fn half_depth(self) -> f32 {
        self.half_depth
    }
    pub fn height(self) -> f32 {
        self.height
    }
}
impl Default for CollisionShape {
    fn default() -> Self {
        Self {
            half_width: PLAYER_RADIUS,
            half_depth: PLAYER_RADIUS,
            height: PLAYER_HEIGHT,
        }
    }
}
