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
    assert!((look_direction(std::f32::consts::FRAC_PI_2, 0.0) - Vec3::NEG_X).length() < 0.00001);
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
