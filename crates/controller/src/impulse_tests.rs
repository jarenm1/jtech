use super::*;
use voxel_world::STONE;

fn player(position: Vec3) -> PlayerState {
    PlayerState {
        position,
        ..Default::default()
    }
}

#[test]
fn upward_impulse_launches_without_jump_then_gravity_applies() {
    let world = tests::arena();
    let mut state = player(Vec3::new(0.5, 0.0, 0.5));
    state.grounded = true;
    apply_player_impulse(&mut state, Vec3::Y * PLAYER_MASS * 12.0);
    assert_eq!(state.velocity.y, 12.0);
    assert!(!state.grounded);
    step_player(&world, &mut state, &PlayerInput::default(), FIXED_DT);
    assert!(state.position.y > 0.0);
    assert!((state.velocity.y - (12.0 - GRAVITY * FIXED_DT)).abs() < 0.00001);
    assert!(!state.grounded);
}

#[test]
fn horizontal_impulse_persists_and_adds_to_motor_input() {
    let world = tests::arena();
    let mut state = player(Vec3::new(0.5, 10.0, 0.5));
    apply_player_impulse(&mut state, Vec3::new(8.0, 0.0, 3.0) * PLAYER_MASS);
    let start = state.position;
    step_player(&world, &mut state, &PlayerInput::default(), FIXED_DT);
    assert!((state.position.x - start.x - 8.0 * FIXED_DT).abs() < 0.00001);
    assert!(state.position.z > start.z);
    let first = state.position;
    let input = PlayerInput {
        movement: [1.0, 0.0],
        ..Default::default()
    };
    step_player(&world, &mut state, &input, FIXED_DT);
    assert!(state.position.x - first.x > SPEED * FIXED_DT);
    assert!(state.position.z > first.z);
    assert!((state.velocity.x - state.external_velocity.x - SPEED).abs() < 0.00001);
    assert!((state.velocity.z - state.external_velocity.y).abs() < 0.00001);
}

#[test]
fn exponential_drag_is_timestep_correct_and_stronger_on_ground() {
    let world = tests::arena();
    for (height, drag) in [(10.0, 1.0_f32), (-0.5, 8.0_f32)] {
        let mut whole = player(Vec3::new(0.5, height, 0.5));
        apply_player_impulse(&mut whole, Vec3::X * PLAYER_MASS * 4.0);
        let mut split = whole;
        step_player(&world, &mut whole, &PlayerInput::default(), 0.2);
        for _ in 0..12 {
            step_player(&world, &mut split, &PlayerInput::default(), FIXED_DT);
        }
        let expected = 4.0 * (-drag * 0.2).exp();
        assert!((whole.external_velocity.x - expected).abs() < 0.00001);
        assert!((split.external_velocity.x - expected).abs() < 0.00001);
        assert!((whole.velocity.x - whole.external_velocity.x).abs() < 0.00001);
    }
}

#[test]
fn terrain_collision_cancels_normal_momentum_but_preserves_sliding() {
    let mut world = tests::arena();
    for y in 0..4 {
        for z in -2..=4 {
            world.set_block(IVec3::new(1, y, z), STONE);
        }
    }
    let mut state = player(Vec3::new(0.5, 1.0, 0.5));
    apply_player_impulse(&mut state, Vec3::new(20.0, 0.0, 2.0) * PLAYER_MASS);
    step_player(&world, &mut state, &PlayerInput::default(), 0.1);
    assert!((state.position.x - 0.7).abs() < 0.00001);
    assert_eq!(state.external_velocity.x, 0.0);
    assert_eq!(state.velocity.x, 0.0);
    assert!(state.external_velocity.y > 0.0);
    assert!(state.position.z > 0.5);
    // Once the wall is gone, canceled momentum must not resume.
    for y in 0..4 {
        for z in -2..=4 {
            world.set_block(IVec3::new(1, y, z), AIR);
        }
    }
    let x = state.position.x;
    step_player(&world, &mut state, &PlayerInput::default(), FIXED_DT);
    assert_eq!(state.position.x, x);
}

#[test]
fn body_sweep_and_overlap_separation_cancel_external_momentum() {
    let world = tests::arena();
    let body = DynamicCollider::cube(1, Vec3::new(0.5, 0.5, 1.5), Vec3::ZERO);
    let mut state = player(Vec3::new(0.5, 0.0, 0.5));
    apply_player_impulse(&mut state, Vec3::Z * PLAYER_MASS * 20.0);
    step_player_with_bodies(&world, &mut state, &PlayerInput::default(), 0.1, &[body]);
    assert!((state.position.z - 0.7).abs() < 0.00001);
    assert_eq!(state.external_velocity, Vec2::ZERO);
    assert_eq!(state.velocity.z, 0.0);

    // A newer body snapshot overlaps the player rather than crossing a sweep.
    state = player(Vec3::new(0.5, 0.0, 0.9));
    apply_player_impulse(&mut state, Vec3::Z * PLAYER_MASS * 20.0);
    step_player_with_bodies(
        &world,
        &mut state,
        &PlayerInput::default(),
        FIXED_DT,
        &[body],
    );
    assert!((state.position.z - 0.7).abs() < 0.00001);
    assert_eq!(state.external_velocity, Vec2::ZERO);
    assert_eq!(state.velocity.z, 0.0);
    let z = state.position.z;
    step_player(&world, &mut state, &PlayerInput::default(), FIXED_DT);
    assert_eq!(state.position.z, z);
}

#[test]
fn repeated_and_extreme_impulses_apply_only_the_capped_change() {
    let mut state = PlayerState::default();
    for _ in 0..100 {
        apply_player_impulse(&mut state, Vec3::new(100.0, 100.0, 100.0) * PLAYER_MASS);
        assert!(state.external_velocity.length() <= 60.00001);
        assert!(state.velocity.y <= 60.0);
        assert_eq!(state.velocity.x, state.external_velocity.x);
        assert_eq!(state.velocity.z, state.external_velocity.y);
    }
    apply_player_impulse(&mut state, Vec3::splat(f32::MAX));
    assert!(state.velocity.is_finite());
    assert!((state.external_velocity.length() - 60.0).abs() < 0.00001);
    apply_player_impulse(&mut state, Vec3::splat(-f32::MAX));
    assert!(state.velocity.is_finite());
    assert_eq!(state.velocity.y, -60.0);
    assert!(state.external_velocity.length() <= 60.00001);
    assert!(state.velocity.x < 0.0 && state.velocity.z < 0.0);
}

#[test]
fn invalid_impulses_are_ignored_and_invalid_external_state_is_sanitized() {
    let world = tests::arena();
    let mut state = player(Vec3::new(0.5, 10.0, 0.5));
    apply_player_impulse(&mut state, Vec3::X * PLAYER_MASS);
    let before = state;
    for impulse in [
        Vec3::new(f32::NAN, 1.0, 1.0),
        Vec3::new(1.0, f32::INFINITY, 1.0),
        Vec3::new(1.0, 1.0, f32::NEG_INFINITY),
    ] {
        apply_player_impulse(&mut state, impulse);
        assert_eq!(state, before);
    }
    state.external_velocity = Vec2::new(f32::NAN, f32::INFINITY);
    step_player(&world, &mut state, &PlayerInput::default(), FIXED_DT);
    assert_eq!(state.external_velocity, Vec2::ZERO);
    assert_eq!(state.position.x, before.position.x);
    assert!(state.velocity.is_finite());
    assert!(state.position.is_finite());

    state.external_velocity = Vec2::splat(f32::NAN);
    state.velocity = Vec3::splat(f32::NAN);
    apply_player_impulse(&mut state, Vec3::Z * PLAYER_MASS);
    assert_eq!(state.external_velocity, Vec2::Y);
    assert_eq!(state.velocity, Vec3::Z);
}

#[test]
fn noclip_discards_momentum_and_ignores_impulses() {
    let world = tests::arena();
    let mut state = player(Vec3::new(0.5, 10.0, 0.5));
    apply_player_impulse(&mut state, Vec3::splat(10.0) * PLAYER_MASS);
    let input = PlayerInput {
        noclip: true,
        ..Default::default()
    };
    step_player(&world, &mut state, &input, FIXED_DT);
    assert!(state.noclip);
    assert_eq!(state.external_velocity, Vec2::ZERO);
    assert_eq!(state.velocity, Vec3::ZERO);
    let flying = state;
    apply_player_impulse(&mut state, Vec3::splat(20.0) * PLAYER_MASS);
    assert_eq!(state, flying);
    step_player(&world, &mut state, &PlayerInput::default(), FIXED_DT);
    assert!(!state.noclip);
    assert_eq!(state.external_velocity, Vec2::ZERO);
    assert_eq!(state.position.x, flying.position.x);
    assert_eq!(state.position.z, flying.position.z);
    assert!(state.velocity.y < 0.0);
}
