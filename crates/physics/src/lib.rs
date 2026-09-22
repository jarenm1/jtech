mod impulse;
use bevy_app::{App, FixedUpdate, Plugin};
use bevy_ecs::schedule::{IntoScheduleConfigs, SystemSet};
use glam::{IVec3, Vec2, Vec3};
pub use impulse::{apply_impulse, apply_player_impulse, bounded_horizontal};
use serde::{Deserialize, Serialize};
use voxel_world::VoxelWorld;

pub const PLAYER_RADIUS: f32 = 0.3;
pub const PLAYER_HEIGHT: f32 = 1.8;
/// Player mass in kilograms, used to convert impulses into velocity changes.
pub const PLAYER_MASS: f32 = 80.0;
pub const EYE_HEIGHT: f32 = 1.6;
pub const FIXED_DT: f32 = 1.0 / 60.0;
const EPSILON: f32 = 0.0001;

/// Latest observed dynamic box, centered at `position`; callers own snapshot
/// timing. `half_extents` is the box half-size on each axis — unit cubes use
/// `Vec3::splat(0.5)`, building pieces use their kind's extents.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DynamicCollider {
    pub id: u32,
    pub position: Vec3,
    pub velocity: Vec3,
    pub half_extents: Vec3,
}
impl DynamicCollider {
    /// Unit cube shorthand for loose voxel bodies.
    pub fn cube(id: u32, position: Vec3, velocity: Vec3) -> Self {
        Self {
            id,
            position,
            velocity,
            half_extents: Vec3::splat(0.5),
        }
    }
    pub fn aabb(&self) -> (Vec3, Vec3) {
        (self.position - self.half_extents, self.position + self.half_extents)
    }
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

/// Ray vs a feet-anchored AABB. Returns the entry distance in world units along
/// the normalized direction, or `None` on a miss. An origin inside the box hits
/// at distance zero so point-blank swings still connect.
pub fn raycast_body(
    origin: Vec3,
    direction: Vec3,
    position: Vec3,
    shape: CollisionShape,
) -> Option<f32> {
    if !origin.is_finite() || !position.is_finite() {
        return None;
    }
    let direction = direction.try_normalize()?;
    let (min, max) = bounds(position, shape);
    let mut enter = 0.0_f32;
    let mut exit = f32::INFINITY;
    for axis in 0..3 {
        let d = direction[axis];
        if d.abs() < EPSILON {
            if origin[axis] < min[axis] || origin[axis] > max[axis] {
                return None;
            }
            continue;
        }
        let inv = d.recip();
        let mut near = (min[axis] - origin[axis]) * inv;
        let mut far = (max[axis] - origin[axis]) * inv;
        if near > far {
            std::mem::swap(&mut near, &mut far);
        }
        enter = enter.max(near);
        exit = exit.min(far);
        if enter > exit {
            return None;
        }
    }
    Some(enter)
}
/// Discrete solids: player-placed cubes and unloaded chunks. Smooth terrain is
/// handled by the density sweep, so terrain voxels are not solid here.
fn solid(world: &VoxelWorld, cell: IVec3) -> bool {
    world.voxel(cell).is_none_or(|v| v.placed)
}

/// Height the feet may ride up per sweep: slopes rising less than this per
/// horizontal move are climbed by the depenetration pass instead of blocking.
/// The motor also snaps grounded actors down by this much to keep support on
/// descents.
pub const STEP_HEIGHT: f32 = 0.55;
/// Density margin counting as contact without blocking, so a face resting on
/// the iso surface still reports a hit for `grounded`.
const CONTACT_SKIN: f32 = 0.02;

/// Sampled points of the leading face: corners plus center. Horizontal sweeps
/// lift the bottom edge by `STEP_HEIGHT`, letting the feet embed shallowly into
/// slopes the vertical pass then climbs out of. `None` (unloaded) samples are
/// treated as air; the discrete scan already stops at unloaded cells.
fn face_samples(position: Vec3, axis: usize, positive: bool, shape: CollisionShape) -> [Vec3; 5] {
    let (min, max) = bounds(position, shape);
    let face = if positive { max[axis] } else { min[axis] };
    let a = (axis + 1) % 3;
    let b = (axis + 2) % 3;
    let mut lo_a = min[a];
    let mut lo_b = min[b];
    if axis != 1 {
        // The face's lower edge is the feet; raise it so shallow slope
        // penetration is allowed and resolved by the vertical pass.
        if a == 1 {
            lo_a += STEP_HEIGHT;
        } else {
            lo_b += STEP_HEIGHT;
        }
    }
    let point = |pa: f32, pb: f32| {
        let mut p = Vec3::ZERO;
        p[axis] = face;
        p[a] = pa;
        p[b] = pb;
        p
    };
    [
        point(lo_a, lo_b),
        point(lo_a, max[b]),
        point(max[a], lo_b),
        point(max[a], max[b]),
        point((lo_a + max[a]) * 0.5, (lo_b + max[b]) * 0.5),
    ]
}

/// Worst density over the face samples: `Some(max)` or `None` when every sample
/// is unloaded. A face is blocked when this exceeds zero.
fn face_density(world: &VoxelWorld, samples: &[Vec3; 5]) -> Option<f32> {
    samples
        .iter()
        .filter_map(|p| world.density_at(*p))
        .reduce(f32::max)
}

/// Density sweep of the leading face along `axis` by `distance`. Returns the
/// allowed displacement and whether the face touched terrain within
/// `CONTACT_SKIN`. Marches in quarter-cell steps so thin features cannot be
/// tunneled, then binary-searches the blocking step.
fn density_sweep(
    world: &VoxelWorld,
    position: Vec3,
    axis: usize,
    distance: f32,
    shape: CollisionShape,
) -> (f32, bool) {
    let positive = distance > 0.0;
    let total = distance.abs();
    let probe = |at: f32| {
        let mut p = position;
        p[axis] += if positive { at } else { -at };
        face_density(world, &face_samples(p, axis, positive, shape))
    };
    let mut contact = false;
    let mut free = 0.0f32;
    let mut blocked_at = f32::NAN;
    while free < total - EPSILON {
        let next = (free + 0.25).min(total);
        match probe(next) {
            Some(d) if d > 0.0 => {
                blocked_at = next;
                break;
            }
            Some(d) => {
                contact |= d > -CONTACT_SKIN;
                free = next;
            }
            None => free = next,
        }
    }
    if blocked_at.is_nan() {
        return (distance, contact);
    }
    // Binary-search the largest displacement keeping every sample at/below zero.
    let (mut lo, mut hi) = (free, blocked_at);
    for _ in 0..10 {
        let mid = (lo + hi) * 0.5;
        match probe(mid) {
            Some(d) if d > 0.0 => hi = mid,
            _ => lo = mid,
        }
    }
    (if positive { lo } else { -lo }, true)
}

/// First displacement in `(0, STEP_HEIGHT]` where the whole body is clear of
/// terrain density and discrete solids, or `None` when deeply embedded.
fn depenetrate_up(world: &VoxelWorld, position: Vec3, shape: CollisionShape) -> Option<f32> {
    let clear = |up: f32| {
        let lifted = position + Vec3::Y * up;
        let (min, max) = bounds(lifted, shape);
        let bottom = face_samples(lifted, 1, false, shape);
        let top = face_samples(lifted, 1, true, shape);
        let dense = face_density(world, &bottom)
            .into_iter()
            .chain(face_density(world, &top))
            .all(|d| d <= 0.0);
        if !dense {
            return false;
        }
        let first = (min + Vec3::splat(EPSILON)).floor().as_ivec3();
        let last = (max - Vec3::splat(EPSILON)).floor().as_ivec3();
        !(first.y..=last.y).any(|y| {
            (first.z..=last.z).any(|z| {
                (first.x..=last.x).any(|x| solid(world, IVec3::new(x, y, z)))
            })
        })
    };
    if clear(0.0) {
        return Some(0.0);
    }
    let mut lo = 0.0f32;
    let mut hi = STEP_HEIGHT;
    if !clear(hi) {
        return None;
    }
    for _ in 0..10 {
        let mid = (lo + hi) * 0.5;
        if clear(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Some(hi)
}

/// Sweeps the leading AABB face across every crossed voxel plane, not just its
/// endpoint, then refines against the smooth density field. Sequential axes
/// preserve tangential displacement when a wall blocks one component.
fn sweep_axis(
    world: &VoxelWorld,
    position: &mut Vec3,
    axis: usize,
    distance: f32,
    shape: CollisionShape,
) -> bool {
    // Feet already inside terrain: climb out vertically within step height.
    // Horizontal axes stay permeable so the vertical pass can resolve it.
    // Upward sweeps skip this: rising carries the face out of the surface.
    if axis == 1
        && distance <= 0.0
        && face_density(world, &face_samples(*position, 1, false, shape))
            .is_some_and(|d| d > 0.0)
        && let Some(up) = depenetrate_up(world, *position, shape)
    {
        position.y += up;
        return true;
    }
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
    let mut allowed = distance;
    let mut hit = false;
    let mut plane = first;
    'planes: while if positive {
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
                    let room = plane as f32 - face;
                    allowed = if positive {
                        room.max(0.0).min(distance)
                    } else {
                        room.min(0.0).max(distance)
                    };
                    hit = true;
                    break 'planes;
                }
            }
        }
        plane += if positive { 1 } else { -1 };
    }
    // The density sweep never moves further than the discrete scan allowed.
    let (smooth, touched) = density_sweep(world, *position, axis, allowed, shape);
    position[axis] += smooth;
    hit || touched || smooth != distance
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
        let (lo, hi) = body.aabb();
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
            let (lo, hi) = body.aabb();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raycast_body_reports_entry_distance_and_misses() {
        let shape = CollisionShape::default();
        let feet = Vec3::new(0.0, 0.0, -5.0);
        // Straight at the torso: enters the 0.3-radius box 4.7m out.
        let hit = raycast_body(Vec3::ZERO, Vec3::NEG_Z, feet, shape).unwrap();
        assert!((hit - 4.7).abs() < 0.001, "{hit}");
        // Above the head and beside the body miss entirely.
        assert!(raycast_body(Vec3::ZERO, Vec3::Y, feet, shape).is_none());
        assert!(raycast_body(Vec3::ZERO, Vec3::X, feet, shape).is_none());
        // Origin inside the box still connects at zero distance.
        assert_eq!(
            raycast_body(feet + Vec3::Y, Vec3::NEG_Z, feet, shape),
            Some(0.0)
        );
        // Non-finite inputs miss rather than panic.
        assert!(raycast_body(Vec3::ZERO, Vec3::ZERO, feet, shape).is_none());
        assert!(raycast_body(Vec3::splat(f32::NAN), Vec3::NEG_Z, feet, shape).is_none());
    }
}
