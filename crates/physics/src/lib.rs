use bevy_app::{App, FixedUpdate, Plugin};
use bevy_ecs::schedule::{IntoScheduleConfigs, SystemSet};
use glam::{IVec3, Vec3};
use serde::{Deserialize, Serialize};
use voxel_world::{AIR, VoxelWorld};

pub const PLAYER_RADIUS: f32 = 0.3;
pub const PLAYER_HEIGHT: f32 = 1.8;
pub const EYE_HEIGHT: f32 = 1.6;
pub const FIXED_DT: f32 = 1.0 / 60.0;
const SPEED: f32 = 6.0;
const GRAVITY: f32 = 24.0;
const JUMP_SPEED: f32 = 8.0;
const EPSILON: f32 = 0.0001;

/// Latest observed unit cube, centered at `position`; callers own snapshot timing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DynamicCollider {
    pub id: u32,
    pub position: Vec3,
    pub velocity: Vec3,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlayerState {
    pub position: Vec3,
    pub velocity: Vec3,
    pub grounded: bool,
}
impl Default for PlayerState {
    fn default() -> Self {
        Self {
            position: Vec3::new(0.5, 24.0, 0.5),
            velocity: Vec3::ZERO,
            grounded: false,
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Default)]
pub struct PlayerInput {
    pub sequence: u64,
    pub movement: [f32; 2],
    pub yaw: f32,
    pub pitch: f32,
    pub jump: bool,
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
fn bounds(position: Vec3) -> (Vec3, Vec3) {
    (
        position - Vec3::new(PLAYER_RADIUS, 0.0, PLAYER_RADIUS),
        position + Vec3::new(PLAYER_RADIUS, PLAYER_HEIGHT, PLAYER_RADIUS),
    )
}
pub fn overlaps_block(state: &PlayerState, block: IVec3) -> bool {
    let (min, max) = bounds(state.position);
    let block_min = block.as_vec3();
    min.cmplt(block_min + Vec3::ONE).all() && max.cmpgt(block_min).all()
}
fn solid(world: &VoxelWorld, cell: IVec3) -> bool {
    world.block(cell) != Some(AIR)
}

/// Sweeps the leading AABB face across every crossed voxel plane, not just its endpoint.
/// Sequential axes preserve tangential displacement when a wall blocks one component.
fn sweep_axis(world: &VoxelWorld, position: &mut Vec3, axis: usize, distance: f32) -> bool {
    if distance == 0.0 {
        return false;
    }
    let (min, max) = bounds(*position);
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
fn sweep_with_bodies(
    world: &VoxelWorld,
    position: &mut Vec3,
    axis: usize,
    distance: f32,
    bodies: &[DynamicCollider],
    ignore: Option<usize>,
) -> bool {
    if distance == 0.0 {
        return false;
    }
    let start = *position;
    let mut hit = sweep_axis(world, position, axis, distance);
    let mut allowed = position[axis] - start[axis];
    let (min, max) = bounds(start);
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
fn separate_bodies(world: &VoxelWorld, state: &mut PlayerState, bodies: &[DynamicCollider]) {
    let mut budget = 1.0_f32;
    for _ in 0..4 {
        let mut corrected = false;
        for (index, body) in bodies.iter().enumerate() {
            if !body.position.is_finite() {
                continue;
            }
            let (min, max) = bounds(state.position);
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
                    sweep_with_bodies(world, &mut candidate, axis, distance, bodies, Some(index));
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
                corrected = true;
            }
        }
        if !corrected || budget <= EPSILON {
            break;
        }
    }
}

pub fn step_player(world: &VoxelWorld, state: &mut PlayerState, input: &PlayerInput, dt: f32) {
    step_player_with_bodies(world, state, input, dt, &[]);
}

/// Shared authoritative/predicted character movement against voxel terrain and
/// loose cubes. Snapshot displacement, not repeated velocity impulses, pushes the
/// character; unchanged snapshots never repeatedly carry or launch the player.
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
    // Bounded catch-up prevents pathological caller timesteps and unbounded collision work.
    let dt = dt.min(0.25);
    let yaw = finite(input.yaw);
    let (sy, cy) = yaw.sin_cos();
    let right = Vec3::new(cy, 0.0, -sy);
    let forward = Vec3::new(-sy, 0.0, -cy);
    let movement = glam::Vec2::new(
        finite(input.movement[0]).clamp(-1.0, 1.0),
        finite(input.movement[1]).clamp(-1.0, 1.0),
    )
    .clamp_length_max(1.0);
    let horizontal = (right * movement.x + forward * movement.y) * SPEED;
    state.velocity.x = horizontal.x;
    state.velocity.z = horizontal.z;
    separate_bodies(world, state, bodies);
    let mut probe = state.position;
    state.grounded =
        state.velocity.y <= 0.0 && sweep_with_bodies(world, &mut probe, 1, -0.002, bodies, None);
    if input.jump && state.grounded {
        state.velocity.y = JUMP_SPEED;
        state.grounded = false;
    }
    state.velocity.y = (state.velocity.y - GRAVITY * dt).clamp(-60.0, 60.0);
    if sweep_with_bodies(
        world,
        &mut state.position,
        0,
        state.velocity.x * dt,
        bodies,
        None,
    ) {
        state.velocity.x = 0.0;
    }
    if sweep_with_bodies(
        world,
        &mut state.position,
        2,
        state.velocity.z * dt,
        bodies,
        None,
    ) {
        state.velocity.z = 0.0;
    }
    let downward = state.velocity.y < 0.0;
    if sweep_with_bodies(
        world,
        &mut state.position,
        1,
        state.velocity.y * dt,
        bodies,
        None,
    ) {
        state.velocity.y = 0.0;
        state.grounded = downward;
    } else {
        state.grounded = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voxel_world::{Chunk, STONE};
    fn arena() -> VoxelWorld {
        let mut world = VoxelWorld::default();
        for x in -1..=0 {
            for z in -1..=0 {
                world.insert(
                    IVec3::new(x, 0, z),
                    Chunk::from_runs(0, &[(32768, AIR)]).unwrap(),
                );
                world.insert(
                    IVec3::new(x, -1, z),
                    Chunk::from_runs(0, &[(32768, STONE)]).unwrap(),
                );
            }
        }
        world
    }
    fn player(position: Vec3) -> PlayerState {
        PlayerState {
            position,
            velocity: Vec3::ZERO,
            grounded: false,
        }
    }
    #[test]
    fn falling_cannot_tunnel_and_resting_contact_supports_jump() {
        let world = arena();
        let mut state = player(Vec3::new(0.5, 10.0, 0.5));
        state.velocity.y = -60.0;
        step_player(&world, &mut state, &PlayerInput::default(), 0.25);
        assert_eq!(state.position.y, 0.0);
        assert!(state.grounded);
        for _ in 0..30 {
            step_player(&world, &mut state, &PlayerInput::default(), FIXED_DT);
        }
        assert_eq!(state.position.y, 0.0);
        let jump = PlayerInput {
            jump: true,
            ..Default::default()
        };
        step_player(&world, &mut state, &jump, FIXED_DT);
        assert!(state.position.y > 0.0 && state.velocity.y > 0.0 && !state.grounded);
        let velocity = state.velocity.y;
        step_player(&world, &mut state, &jump, FIXED_DT);
        assert!(state.velocity.y < velocity);
    }
    #[test]
    fn thin_wall_stops_sweep_but_keeps_sliding() {
        let mut world = arena();
        for y in 0..3 {
            for z in -4..=4 {
                world.set_block(IVec3::new(1, y, z), STONE);
            }
        }
        let mut state = player(Vec3::new(0.5, 0.0, 0.5));
        let input = PlayerInput {
            movement: [1.0, 1.0],
            ..Default::default()
        };
        step_player(&world, &mut state, &input, 0.25);
        assert!((state.position.x - 0.7).abs() < 0.00001);
        assert!(state.position.z < 0.0);
        assert!(state.grounded);
        assert!(!overlaps_block(&state, IVec3::new(1, 0, 0)));
    }
    #[test]
    fn negative_seam_is_traversable_but_missing_chunk_is_solid() {
        let mut world = arena();
        let input = PlayerInput {
            movement: [-1.0, 0.0],
            ..Default::default()
        };
        let mut state = player(Vec3::new(0.5, 0.0, 0.5));
        step_player(&world, &mut state, &input, 0.25);
        assert!(state.position.x < -0.3);
        assert!(state.grounded);
        world.remove(IVec3::new(-1, 0, 0));
        state = player(Vec3::new(0.5, 0.0, 0.5));
        step_player(&world, &mut state, &input, 0.25);
        assert!((state.position.x - PLAYER_RADIUS).abs() < 0.00001);
    }
    #[test]
    fn ceiling_blocks_jump_and_invalid_input_cannot_poison_state() {
        let mut world = arena();
        world.set_block(IVec3::new(0, 2, 0), STONE);
        let mut state = player(Vec3::new(0.5, 0.0, 0.5));
        step_player(
            &world,
            &mut state,
            &PlayerInput {
                jump: true,
                ..Default::default()
            },
            0.1,
        );
        assert!((state.position.y - 0.2).abs() < 0.00001);
        assert_eq!(state.velocity.y, 0.0);
        assert!(!state.grounded);
        step_player(
            &world,
            &mut state,
            &PlayerInput {
                movement: [f32::NAN, f32::INFINITY],
                yaw: f32::NAN,
                ..Default::default()
            },
            FIXED_DT,
        );
        assert!(state.position.is_finite());
        assert!(state.velocity.is_finite());
        assert!((look_direction(0.0, 0.0) - Vec3::NEG_Z).length() < 0.00001);
        assert!(
            (look_direction(std::f32::consts::FRAC_PI_2, 0.0) - Vec3::NEG_X).length() < 0.00001
        );
    }

    fn body(position: Vec3, velocity: Vec3) -> DynamicCollider {
        DynamicCollider {
            id: 1,
            position,
            velocity,
        }
    }

    #[test]
    fn loose_cube_sweeps_stop_side_top_and_ceiling() {
        let world = arena();
        let bodies = [body(Vec3::new(1.5, 0.5, 0.5), Vec3::ZERO)];
        let mut state = player(Vec3::new(0.5, 0.0, 0.5));
        let input = PlayerInput {
            movement: [1.0, 0.0],
            ..Default::default()
        };
        step_player_with_bodies(&world, &mut state, &input, 0.25, &bodies);
        assert!((state.position.x - 0.7).abs() < EPSILON);
        assert_eq!(state.velocity.x, 0.0);

        state = player(Vec3::new(1.5, 10.0, 0.5));
        state.velocity.y = -60.0;
        step_player_with_bodies(&world, &mut state, &PlayerInput::default(), 0.25, &bodies);
        assert!((state.position.y - 1.0).abs() < EPSILON);
        assert!(state.grounded);

        let ceiling = [body(Vec3::new(0.5, 2.5, 0.5), Vec3::ZERO)];
        state = player(Vec3::new(0.5, 0.0, 0.5));
        let jump = PlayerInput {
            jump: true,
            ..Default::default()
        };
        step_player_with_bodies(&world, &mut state, &jump, 0.1, &ceiling);
        assert!((state.position.y - 0.2).abs() < EPSILON);
        assert_eq!(state.velocity.y, 0.0);
        assert!(!state.grounded);
    }

    #[test]
    fn loose_support_is_stable_and_allows_jump_without_snapshot_drift() {
        let world = arena();
        let bodies = [body(Vec3::new(0.5, 0.5, 0.5), Vec3::new(4.0, 2.0, 0.0))];
        let mut state = player(Vec3::new(0.5, 1.0, 0.5));
        for _ in 0..60 {
            step_player_with_bodies(
                &world,
                &mut state,
                &PlayerInput::default(),
                FIXED_DT,
                &bodies,
            );
            assert!(state.grounded);
            assert_eq!(state.position, Vec3::new(0.5, 1.0, 0.5));
        }
        let jump = PlayerInput {
            jump: true,
            ..Default::default()
        };
        step_player_with_bodies(&world, &mut state, &jump, FIXED_DT, &bodies);
        assert!(state.position.y > 1.0 && state.velocity.y > 0.0 && !state.grounded);
    }

    #[test]
    fn updated_body_snapshot_pushes_once_and_cannot_cross_terrain() {
        let mut world = arena();
        let mut bodies = [body(Vec3::new(0.5, 0.5, 0.5), Vec3::X * 5.0)];
        let mut state = player(Vec3::new(1.3, 0.0, 0.5));
        bodies[0].position.x += 0.2;
        step_player_with_bodies(
            &world,
            &mut state,
            &PlayerInput::default(),
            FIXED_DT,
            &bodies,
        );
        assert!((state.position.x - 1.5).abs() < EPSILON);
        let pushed = state.position;
        for _ in 0..10 {
            step_player_with_bodies(
                &world,
                &mut state,
                &PlayerInput::default(),
                FIXED_DT,
                &bodies,
            );
            assert_eq!(state.position, pushed);
        }
        for y in 0..3 {
            for z in -2..=2 {
                world.set_block(IVec3::new(2, y, z), STONE);
            }
        }
        bodies[0].position.x = 1.2;
        step_player_with_bodies(
            &world,
            &mut state,
            &PlayerInput::default(),
            FIXED_DT,
            &bodies,
        );
        assert!(state.position.x <= 2.0 - PLAYER_RADIUS + EPSILON);
        assert!(state.position.y >= 0.0);
        for y in 0..3 {
            for z in -2..=2 {
                assert!(!overlaps_block(&state, IVec3::new(2, y, z)));
            }
        }
    }

    #[test]
    fn falling_body_overlap_does_not_push_character_through_floor() {
        let world = arena();
        let bodies = [body(Vec3::new(0.5, 2.0, 0.5), Vec3::NEG_Y * 10.0)];
        let mut state = player(Vec3::new(0.5, 0.0, 0.5));
        for _ in 0..10 {
            step_player_with_bodies(
                &world,
                &mut state,
                &PlayerInput::default(),
                FIXED_DT,
                &bodies,
            );
            assert!(state.position.y >= 0.0);
            let (min, max) = bounds(state.position);
            assert!(
                !(min
                    .cmplt(bodies[0].position + Vec3::splat(0.5 - EPSILON))
                    .all()
                    && max
                        .cmpgt(bodies[0].position - Vec3::splat(0.5 - EPSILON))
                        .all())
            );
        }
    }
}
