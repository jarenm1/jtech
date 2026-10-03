use super::*;
use voxel_world::{Chunk, STONE};
pub(super) fn arena() -> VoxelWorld {
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
    // The smooth iso surface between the stone layer and air rests near
    // y = -0.5, halfway between the lattice samples.
    world
}
fn player(position: Vec3) -> PlayerState {
    PlayerState {
        position,
        velocity: Vec3::ZERO,
        external_velocity: Vec2::ZERO,
        grounded: false,
        noclip: false,
    }
}
#[test]
fn falling_cannot_tunnel_and_resting_contact_supports_jump() {
    let world = arena();
    let mut state = player(Vec3::new(0.5, 10.0, 0.5));
    state.velocity.y = -60.0;
    step_player(&world, &mut state, &PlayerInput::default(), 0.25);
    // The smooth floor's iso surface sits halfway between lattice samples.
    assert!((state.position.y + 0.5).abs() < 0.01);
    assert!(state.grounded);
    for _ in 0..30 {
        step_player(&world, &mut state, &PlayerInput::default(), FIXED_DT);
    }
    assert!((state.position.y + 0.5).abs() < 0.01);
    let jump = PlayerInput {
        jump: true,
        ..Default::default()
    };
    step_player(&world, &mut state, &jump, FIXED_DT);
    assert!(state.position.y > -0.5 && state.velocity.y > 0.0 && !state.grounded);
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
    let mut state = player(Vec3::new(0.5, -0.5, 0.5));
    // Two ticks: the jump apex from the smooth floor only crosses the ceiling
    // plane on the second step.
    for _ in 0..2 {
        step_player(
            &world,
            &mut state,
            &PlayerInput {
                jump: true,
                ..Default::default()
            },
            0.1,
        );
    }
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
    assert!((look_direction(std::f32::consts::FRAC_PI_2, 0.0) - Vec3::NEG_X).length() < 0.00001);
}

fn body(position: Vec3, velocity: Vec3) -> DynamicCollider {
    DynamicCollider::cube(1, position, velocity)
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
    state = player(Vec3::new(0.5, -0.5, 0.5));
    let jump = PlayerInput {
        jump: true,
        ..Default::default()
    };
    for _ in 0..2 {
        step_player_with_bodies(&world, &mut state, &jump, 0.1, &ceiling);
    }
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
    let mut state = player(Vec3::new(1.3, -0.5, 0.5));
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
        assert!(state.position.distance(pushed) < 0.01);
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
    assert!(state.position.y >= -0.51);
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
    let mut state = player(Vec3::new(0.5, -0.5, 0.5));
    for _ in 0..10 {
        step_player_with_bodies(
            &world,
            &mut state,
            &PlayerInput::default(),
            FIXED_DT,
            &bodies,
        );
        assert!(state.position.y >= -0.51);
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

/// Smooth ramp rising +X at 0.3 m per metre: density is a linear field so the
/// iso surface is the plane y = 0.3x, not a voxel boundary.
fn ramp() -> VoxelWorld {
    let mut world = VoxelWorld::default();
    for x in -1..=1 {
        for z in -1..=0 {
            world.insert(
                IVec3::new(x, 0, z),
                Chunk::from_runs(0, &[(32768, AIR)]).unwrap(),
            );
            world.insert(
                IVec3::new(x, -1, z),
                Chunk::from_runs(0, &[(32768, AIR)]).unwrap(),
            );
        }
    }
    for x in -4..16 {
        for y in -4..8 {
            for z in -4..6 {
                let density = ((0.3 * x as f32 - y as f32) * 100.0).clamp(-128.0, 127.0) as i8;
                world.set_voxel(
                    IVec3::new(x, y, z),
                    voxel_world::Voxel {
                        material: if density > 0 { STONE } else { AIR },
                        density,
                        placed: false,
                    },
                );
            }
        }
    }
    world
}

#[test]
fn smooth_ramp_is_walkable_and_reports_grounded() {
    let world = ramp();
    // A box on a slope rests on its leading corner: settled height is the
    // surface at x + radius, 0.09 above the center's surface point.
    let mut state = player(Vec3::new(0.5, 0.3 * 0.8, 0.5));
    let input = PlayerInput {
        movement: [1.0, 0.0],
        ..Default::default()
    };
    let mut last_y = state.position.y;
    for _ in 0..60 {
        step_player(&world, &mut state, &input, FIXED_DT);
        assert!(state.grounded);
        // Rises smoothly: no step-up jumps, never sinks back downhill.
        assert!(state.position.y >= last_y - 0.01);
        assert!(state.position.y <= last_y + 0.1);
        last_y = state.position.y;
    }
    assert!(state.position.x > 5.0);
    assert!(state.position.y > 1.0);
    assert!((state.position.y - 0.3 * state.position.x).abs() < 0.15);
}

#[test]
fn sweep_lands_on_the_iso_surface_not_the_voxel_plane() {
    let world = ramp();
    let mut position = Vec3::new(3.5, 4.0, 0.5);
    let hit = physics::sweep_with_bodies(
        &world,
        &mut position,
        1,
        -4.0,
        &physics::BodyBroadphase::new(&[]),
        None,
        physics::CollisionShape::default(),
    );
    assert!(hit);
    // The face's highest sample is the upslope corner at x = 3.8, so the feet
    // rest on the tilted iso plane, not on any integer cell boundary.
    assert!((position.y - 0.3 * 3.8).abs() < 0.1, "{}", position.y);
}
