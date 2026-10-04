use super::*;
use bevy_app::{App, FixedUpdate};
use bevy_ecs::prelude::*;
use voxel_world::STONE;

fn actor(position: Vec3) -> CharacterState {
    CharacterState {
        motion: PlayerState {
            position,
            ..Default::default()
        },
        yaw: 0.0,
        ..Default::default()
    }
}
fn animal() -> (CharacterBody, MovementProfile) {
    (
        CharacterBody::new(CollisionShape::new(0.2, 0.6, 0.6).unwrap(), 20.0).unwrap(),
        MovementProfile {
            speed: 3.0,
            acceleration: 6.0,
            braking: 9.0,
            strafe: 0.0,
            turn_rate: 1.0,
            jump_speed: 0.0,
            ..Default::default()
        },
    )
}
fn tick(
    world: &VoxelWorld,
    state: &mut CharacterState,
    body: &CharacterBody,
    profile: &MovementProfile,
    intent: &mut CharacterIntent,
) {
    step_character(world, state, body, profile, intent, FIXED_DT, &[]);
}

#[test]
fn short_animal_passes_under_ceiling_that_blocks_human() {
    let mut world = tests::arena();
    for x in -2..=2 {
        world.set_block(IVec3::new(x, 1, -1), STONE);
    }
    let start = Vec3::new(0.5, 0.0, 1.5);
    let mut human = actor(start);
    let mut critter = actor(start);
    let (body, profile) = animal();
    let mut intent = CharacterIntent {
        movement: Vec2::Y,
        ..Default::default()
    };
    for _ in 0..90 {
        tick(
            &world,
            &mut human,
            &CharacterBody::default(),
            &MovementProfile::default(),
            &mut intent,
        );
        tick(&world, &mut critter, &body, &profile, &mut intent);
    }
    assert!((human.motion.position.z - 0.3).abs() < EPSILON);
    assert!(critter.motion.position.z < -1.0);
    assert!((critter.motion.position.y + 0.5).abs() < 0.01);
}

#[test]
fn rectangular_body_sweeps_use_each_extent_against_voxels_and_cubes() {
    let (body, _) = animal();
    let mut world = tests::arena();
    for y in 0..3 {
        for z in -2..=2 {
            world.set_block(IVec3::new(1, y, z), STONE);
        }
    }
    let profile = MovementProfile::default();
    let mut state = actor(Vec3::new(0.5, 0.0, 0.5));
    let mut right = CharacterIntent {
        movement: Vec2::X,
        ..Default::default()
    };
    step_character(&world, &mut state, &body, &profile, &mut right, 0.25, &[]);
    assert!((state.motion.position.x - 0.8).abs() < EPSILON);
    let mut forward = CharacterIntent {
        movement: Vec2::Y,
        ..Default::default()
    };
    let cubes = [DynamicCollider::cube(1, Vec3::new(0.5, 0.5, -1.5), Vec3::ZERO)];
    step_character(
        &world,
        &mut state,
        &body,
        &profile,
        &mut forward,
        0.25,
        &cubes,
    );
    assert!((state.motion.position.z + 0.4).abs() < EPSILON);
}

#[test]
fn animal_profile_limits_acceleration_strafe_turn_and_jump() {
    let world = tests::arena();
    let (body, profile) = animal();
    let mut state = actor(Vec3::new(0.5, 0.0, 0.5));
    let mut intent = CharacterIntent {
        movement: Vec2::X,
        jump: true,
        ..Default::default()
    };
    tick(&world, &mut state, &body, &profile, &mut intent);
    assert_eq!(state.motion.position.x, 0.5);
    assert_eq!(state.motion.position.z, 0.5);
    assert!(!intent.jump);
    intent.movement = Vec2::Y;
    intent.turn = 1000.0;
    tick(&world, &mut state, &body, &profile, &mut intent);
    assert!((state.yaw - FIXED_DT).abs() < 1e-6);
    let speed = Vec2::new(state.motion.velocity.x, state.motion.velocity.z).length();
    assert!((speed - profile.acceleration * FIXED_DT).abs() < 1e-6);
    assert!(state.motion.position.x < 0.5);
    assert!(state.motion.position.z < 0.5);
}

#[test]
fn held_movement_persists_but_jump_is_consumed_without_retrigger() {
    let world = tests::arena();
    let mut state = actor(Vec3::new(0.5, 0.0, 0.5));
    let mut intent = CharacterIntent {
        movement: Vec2::X * 0.1,
        jump: true,
        ..Default::default()
    };
    for _ in 0..120 {
        tick(
            &world,
            &mut state,
            &CharacterBody::default(),
            &MovementProfile::default(),
            &mut intent,
        );
    }
    assert!(state.motion.grounded);
    assert!((state.motion.position.y + 0.5).abs() < 0.01);
    assert!((state.motion.position.x - 1.7).abs() < 0.0001);
    assert!(!intent.jump);
    assert_eq!(intent.movement, Vec2::X * 0.1);
}

#[test]
fn body_mass_changes_impulse_and_collision_cancels_normal_momentum() {
    let mut world = tests::arena();
    for y in 0..3 {
        world.set_block(IVec3::new(1, y, 0), STONE);
    }
    let mut human = actor(Vec3::new(0.5, 0.0, 0.5));
    let mut critter = human;
    let (body, profile) = animal();
    human.apply_impulse(&CharacterBody::default(), Vec3::X * 80.0);
    critter.apply_impulse(&body, Vec3::X * 80.0);
    assert_eq!(human.motion.external_velocity.x, 1.0);
    assert_eq!(critter.motion.external_velocity.x, 4.0);
    for _ in 0..30 {
        tick(
            &world,
            &mut critter,
            &body,
            &profile,
            &mut CharacterIntent::default(),
        );
    }
    assert!((critter.motion.position.x - 0.8).abs() < EPSILON);
    assert_eq!(critter.motion.external_velocity.x, 0.0);
}

#[test]
fn invalid_configuration_actions_and_ticks_are_bounded() {
    assert!(CollisionShape::new(f32::NAN, 0.3, 1.0).is_none());
    assert!(CollisionShape::new(0.0, 0.3, 1.0).is_none());
    assert!(CollisionShape::new(100.0, 0.3, 1.0).is_none());
    assert!(CharacterBody::new(CollisionShape::default(), 0.0).is_none());
    assert!(CharacterBody::new(CollisionShape::default(), f32::INFINITY).is_none());
    let world = tests::arena();
    let mut state = actor(Vec3::new(0.5, 0.0, 0.5));
    let original = state;
    let mut intent = CharacterIntent {
        movement: Vec2::new(f32::NAN, f32::MAX),
        turn: f32::INFINITY,
        jump: true,
        attack: false,
        held_item: 0,
        ..Default::default()
    };
    tick(
        &world,
        &mut state,
        &CharacterBody::default(),
        &MovementProfile {
            speed: f32::INFINITY,
            gravity: f32::NAN,
            jump_speed: -1.0,
            ..Default::default()
        },
        &mut intent,
    );
    assert_eq!(state.motion.position, original.motion.position);
    assert_eq!(state.yaw, 0.0);
    assert!(state.motion.velocity.is_finite());
    let before = state;
    intent.jump = true;
    step_character(
        &world,
        &mut state,
        &CharacterBody::default(),
        &MovementProfile::default(),
        &mut intent,
        f32::NAN,
        &[],
    );
    assert_eq!(state, before);
    assert!(intent.jump); // No tick occurred.
    let extreme = CharacterIntent {
        movement: Vec2::splat(f32::MAX),
        turn: -100.0,
        jump: false,
        attack: false,
        held_item: 0,
        ..Default::default()
    }
    .bounded();
    assert!(extreme.movement.length() <= 1.0);
    assert_eq!(extreme.turn, -1.0);
}

#[test]
fn sprint_scales_target_speed_by_the_profile_multiplier() {
    let world = tests::arena();
    let (body, profile) = animal();
    let profile = MovementProfile {
        sprint_mult: 2.0,
        ..profile
    };
    let run = |sprint: bool| {
        let mut state = actor(Vec3::new(0.5, -0.5, 0.5));
        let mut intent = CharacterIntent {
            movement: Vec2::Y,
            sprint,
            ..Default::default()
        };
        for _ in 0..90 {
            tick(&world, &mut state, &body, &profile, &mut intent);
        }
        state.motion.velocity.z
    };
    let walk = run(false);
    let sprint = run(true);
    assert!(
        (sprint / walk - 2.0).abs() < 1e-3,
        "walk {walk} sprint {sprint}"
    );
}

#[test]
fn buffered_jump_fires_on_landing() {
    let world = tests::arena();
    let body = CharacterBody::default();
    let profile = MovementProfile::default();
    let mut state = actor(Vec3::new(0.5, -0.45, 0.5));
    let mut intent = CharacterIntent {
        jump: true,
        ..Default::default()
    };
    tick(&world, &mut state, &body, &profile, &mut intent);
    assert!(
        state.motion.velocity.y <= 0.0,
        "an airborne press must not jump immediately"
    );
    for _ in 0..10 {
        tick(&world, &mut state, &body, &profile, &mut intent);
    }
    assert!(
        state.motion.velocity.y > 0.0,
        "the buffered press must fire on landing"
    );
}

#[test]
fn coyote_time_allows_a_jump_just_after_leaving_the_ground() {
    let world = tests::arena();
    let body = CharacterBody::default();
    let profile = MovementProfile::default();
    let mut state = actor(Vec3::new(0.5, -0.5, 0.5));
    let mut idle = CharacterIntent::default();
    tick(&world, &mut state, &body, &profile, &mut idle);
    assert!(state.motion.grounded && state.coyote > 0);
    // Step off the edge: airborne, but still inside the coyote window.
    state.motion.position.y = 0.0;
    let mut jump = CharacterIntent {
        jump: true,
        ..Default::default()
    };
    tick(&world, &mut state, &body, &profile, &mut jump);
    assert!(
        state.motion.velocity.y > 0.0,
        "a press inside the coyote window must still jump"
    );
}

#[test]
fn grounded_actor_steps_over_a_low_obstruction() {
    let world = tests::arena();
    let body = CharacterBody::default();
    let profile = MovementProfile::default();
    // A loose cube in front whose top sits 0.5 m above the iso surface.
    let bodies = [DynamicCollider::cube(1, Vec3::new(1.5, -0.5, 0.5), Vec3::ZERO)];
    let mut state = actor(Vec3::new(0.5, -0.5, 0.5));
    let mut intent = CharacterIntent {
        movement: Vec2::X,
        ..Default::default()
    };
    let mut climbed = f32::MIN;
    for _ in 0..60 {
        step_character(
            &world,
            &mut state,
            &body,
            &profile,
            &mut intent,
            FIXED_DT,
            &bodies,
        );
        climbed = climbed.max(state.motion.position.y);
    }
    assert!(
        state.motion.position.x > 1.0,
        "actor must pass the cube: {:?}",
        state.motion.position
    );
    assert!(
        climbed > -0.1,
        "actor must climb onto the cube top: {climbed}"
    );
}

#[test]
fn crouch_lowers_the_body_under_a_low_ceiling() {
    let mut world = tests::arena();
    // A ceiling one cell above the floor: standing height does not fit.
    for x in 1..6 {
        for z in -1..=1 {
            world.set_block(IVec3::new(x, 1, z), STONE);
        }
    }
    let body = CharacterBody::default();
    let profile = MovementProfile::default();
    let mut state = actor(Vec3::new(0.5, -0.5, 0.5));
    let mut intent = CharacterIntent {
        movement: Vec2::X,
        crouch: true,
        ..Default::default()
    };
    for _ in 0..60 {
        tick(&world, &mut state, &body, &profile, &mut intent);
    }
    assert!(state.crouching);
    assert!(
        state.motion.position.x > 1.0,
        "crouched actor must pass under the ceiling: {:?}",
        state.motion.position
    );
    // The same ceiling blocks a standing actor.
    let mut standing = actor(Vec3::new(0.5, -0.5, 0.5));
    let mut walk = CharacterIntent {
        movement: Vec2::X,
        ..Default::default()
    };
    for _ in 0..60 {
        tick(&world, &mut standing, &body, &profile, &mut walk);
    }
    assert!(
        standing.motion.position.x < 1.0,
        "standing actor must be blocked: {:?}",
        standing.motion.position
    );
}

#[test]
fn status_effects_compose_by_strongest_wins_and_expire() {
    let mut list = StatusList::default();
    list.apply(StatusKind::Slow, 10);
    list.apply(StatusKind::Slow, 4);
    assert_eq!(list.entries()[0].unwrap().remaining, 10, "shorter refresh keeps the longer");
    list.apply(StatusKind::Slow, 20);
    assert_eq!(list.entries()[0].unwrap().remaining, 20, "longer refresh replaces");
    list.apply(StatusKind::Stun, 3);
    let constraints = list.constraints();
    assert!(constraints.movement_locked && constraints.action_locked);
    assert_eq!(constraints.speed_mult, 0.5);
    for _ in 0..3 {
        list.tick();
    }
    assert!(!list.constraints().action_locked, "stun must expire");
    assert_eq!(list.constraints().speed_mult, 0.5, "slow must persist");
}

#[test]
fn stun_locks_movement_and_jump() {
    let world = tests::arena();
    let body = CharacterBody::default();
    let profile = MovementProfile::default();
    let mut state = actor(Vec3::new(0.5, -0.5, 0.5));
    state.statuses.apply(StatusKind::Stun, 30);
    let mut intent = CharacterIntent {
        movement: Vec2::X,
        jump: true,
        ..Default::default()
    };
    for _ in 0..20 {
        tick(&world, &mut state, &body, &profile, &mut intent);
    }
    assert!(
        (state.motion.position.x - 0.5).abs() < 0.01,
        "stunned actor must not move: {:?}",
        state.motion.position
    );
    assert!(
        state.motion.velocity.y <= 0.0,
        "stunned actor must not jump: {}",
        state.motion.velocity.y
    );
}

fn ability_body(
    kind: AbilityKind,
    behavior: CastBehavior,
    cast_ticks: u16,
    cooldown_ticks: u16,
    magnitude: f32,
) -> CharacterBody {
    CharacterBody::default().with_abilities(AbilityTable::new([
        Some(AbilitySpec {
            kind,
            behavior,
            cast_ticks,
            cooldown_ticks,
            magnitude,
        }),
        None,
        None,
    ]))
}

#[test]
fn instant_dash_moves_along_the_aim_and_starts_a_cooldown() {
    let world = tests::arena();
    let body = ability_body(AbilityKind::Dash, CastBehavior::Instant, 0, 60, 12.0);
    let profile = MovementProfile::default();
    let mut state = actor(Vec3::new(0.5, -0.5, 0.5));
    let mut intent = CharacterIntent {
        ability: [true, false, false],
        aim: Vec2::X,
        ..Default::default()
    };
    let output = step_character(&world, &mut state, &body, &profile, &mut intent, FIXED_DT, &[]);
    assert!(output.fired[0].is_some(), "slot 0 must fire");
    assert!(state.cooldowns[0] > 0, "cooldown must start");
    assert!(state.dash.is_some(), "dash must be active");
    for _ in 0..20 {
        step_character(&world, &mut state, &body, &profile, &mut intent, FIXED_DT, &[]);
    }
    assert!(
        state.motion.position.x > 1.0,
        "dash must move the actor: {:?}",
        state.motion.position
    );
}

#[test]
fn root_cast_locks_movement_then_fires() {
    let world = tests::arena();
    let body = ability_body(AbilityKind::Projectile, CastBehavior::Root, 10, 30, 0.0);
    let profile = MovementProfile::default();
    let mut state = actor(Vec3::new(0.5, -0.5, 0.5));
    let mut intent = CharacterIntent {
        movement: Vec2::X,
        ability: [true, false, false],
        ..Default::default()
    };
    let output = step_character(&world, &mut state, &body, &profile, &mut intent, FIXED_DT, &[]);
    assert!(output.fired[0].is_none(), "cast must not fire immediately");
    assert!(state.cast.is_some());
    for _ in 0..9 {
        step_character(&world, &mut state, &body, &profile, &mut intent, FIXED_DT, &[]);
    }
    assert!(
        (state.motion.position.x - 0.5).abs() < 0.01,
        "rooted caster must not move: {:?}",
        state.motion.position
    );
    let output = step_character(&world, &mut state, &body, &profile, &mut intent, FIXED_DT, &[]);
    assert!(output.fired[0].is_some(), "cast must fire on completion");
    assert!(state.cast.is_none());
}

#[test]
fn ability_on_cooldown_is_ignored() {
    let world = tests::arena();
    let body = ability_body(AbilityKind::Dash, CastBehavior::Instant, 0, 60, 12.0);
    let profile = MovementProfile::default();
    let mut state = actor(Vec3::new(0.5, -0.5, 0.5));
    let mut intent = CharacterIntent {
        ability: [true, false, false],
        ..Default::default()
    };
    step_character(&world, &mut state, &body, &profile, &mut intent, FIXED_DT, &[]);
    let cooldown = state.cooldowns[0];
    let output = step_character(&world, &mut state, &body, &profile, &mut intent, FIXED_DT, &[]);
    assert!(output.fired[0].is_none(), "cooldown must block the second press");
    assert!(state.cooldowns[0] < cooldown, "cooldown must tick down");
}

#[test]
fn basic_attack_fires_once_per_press_and_cooldown() {
    let world = tests::arena();
    let body = CharacterBody::default().with_basic_attack(Some(BasicAttack {
        kind: BasicAttackKind::Melee,
        cooldown_ticks: 30,
        charge_ticks: 0,
    }));
    let profile = MovementProfile::default();
    let mut state = actor(Vec3::new(0.5, -0.5, 0.5));
    let mut intent = CharacterIntent {
        attack: true,
        ..Default::default()
    };
    let output = step_character(&world, &mut state, &body, &profile, &mut intent, FIXED_DT, &[]);
    assert!(output.basic_attack, "the first press must fire");
    assert_eq!(state.basic_attack_cooldown, 30);
    // Holding the attack must not repeat: a melee weapon swings once per press.
    for _ in 0..30 {
        let output = step_character(&world, &mut state, &body, &profile, &mut intent, FIXED_DT, &[]);
        assert!(!output.basic_attack, "a held attack must not repeat");
    }
    // Releasing and pressing again fires once the cooldown has elapsed.
    intent.attack = false;
    step_character(&world, &mut state, &body, &profile, &mut intent, FIXED_DT, &[]);
    intent.attack = true;
    assert!(
        step_character(&world, &mut state, &body, &profile, &mut intent, FIXED_DT, &[]).basic_attack,
        "a fresh press must fire once ready"
    );
}

#[test]
fn no_basic_attack_without_a_weapon() {
    let world = tests::arena();
    let body = CharacterBody::default();
    let profile = MovementProfile::default();
    let mut state = actor(Vec3::new(0.5, -0.5, 0.5));
    let mut intent = CharacterIntent {
        attack: true,
        ..Default::default()
    };
    let output = step_character(&world, &mut state, &body, &profile, &mut intent, FIXED_DT, &[]);
    assert!(!output.basic_attack, "an unarmed body must not swing");
}

#[test]
fn replay_restores_complete_motor_state_and_matches_human_adapter() {
    let world = tests::arena();
    let mut authority = actor(Vec3::new(0.5, 0.0, 0.5));
    let mut predicted = authority.motion;
    let body = CharacterBody::default();
    let profile = MovementProfile::default();
    let mut saved = authority;
    let commands: Vec<_> = (0..90)
        .map(|sequence| PlayerInput {
            sequence,
            movement: [0.15, 0.1],
            yaw: sequence as f32 * 0.02,
            jump: sequence == 3,
            ..Default::default()
        })
        .collect();
    for input in &commands {
        if input.sequence == 30 {
            saved = authority;
        }
        if input.sequence == 45 {
            authority.apply_impulse(&body, Vec3::new(40.0, 80.0, 0.0));
            physics::apply_player_impulse(&mut predicted, Vec3::new(40.0, 80.0, 0.0));
        }
        authority.yaw = input.yaw;
        let mut intent = CharacterIntent {
            movement: Vec2::from_array(input.movement),
            jump: input.jump,
            ..Default::default()
        };
        tick(&world, &mut authority, &body, &profile, &mut intent);
        step_player(&world, &mut predicted, input, FIXED_DT);
        assert_eq!(authority.motion, predicted);
    }
    for input in &commands[30..] {
        if input.sequence == 45 {
            saved.apply_impulse(&body, Vec3::new(40.0, 80.0, 0.0));
        }
        saved.yaw = input.yaw;
        tick(
            &world,
            &mut saved,
            &body,
            &profile,
            &mut CharacterIntent {
                movement: Vec2::from_array(input.movement),
                jump: input.jump,
                ..Default::default()
            },
        );
    }
    assert_eq!(saved, authority);
}

#[derive(Component)]
struct Scripted;
fn script(mut actors: Query<&mut CharacterIntent, With<Scripted>>) {
    for mut intent in &mut actors {
        intent.movement = Vec2::Y;
        intent.turn = 0.25;
    }
}
#[test]
fn headless_plugin_runs_script_before_motor_and_matches_direct_ticks() {
    let mut app = App::new();
    app.add_plugins(ControllerPlugin::default());
    app.insert_resource(tests::arena());
    app.add_systems(FixedUpdate, script.in_set(ControllerSet::Intent));
    let (body, profile) = animal();
    let initial = actor(Vec3::new(0.5, 0.0, 0.5));
    let id = app
        .world_mut()
        .spawn((Scripted, initial, body, profile, CharacterIntent::default()))
        .id();
    let mut direct = initial;
    for _ in 0..60 {
        app.world_mut().run_schedule(FixedUpdate);
        tick(
            app.world().resource::<VoxelWorld>(),
            &mut direct,
            &body,
            &profile,
            &mut CharacterIntent {
                movement: Vec2::Y,
                turn: 0.25,
                jump: false,
                attack: false,
                held_item: 0,
                ..Default::default()
            },
        );
    }
    assert_eq!(*app.world().get::<CharacterState>(id).unwrap(), direct);
    assert!(direct.motion.position.distance(initial.motion.position) > 2.0);
    assert!((direct.yaw - 0.25).abs() < 1e-5);
}

#[test]
fn zero_gravity_retains_support_but_clears_it_after_leaving_edge() {
    let mut world = tests::arena();
    // The pit must clear the actor's trilinear footprint (cells x/z 0 and 1)
    // at the start, then remove support once it walks over it.
    for x in 2..5 {
        for z in -1..=1 {
            world.set_block(IVec3::new(x, -1, z), AIR);
        }
    }
    let mut state = actor(Vec3::new(0.5, -0.5, 0.5));
    let profile = MovementProfile {
        gravity: 0.0,
        ..Default::default()
    };
    tick(
        &world,
        &mut state,
        &CharacterBody::default(),
        &profile,
        &mut CharacterIntent::default(),
    );
    assert!(state.motion.grounded);
    step_character(
        &world,
        &mut state,
        &CharacterBody::default(),
        &profile,
        &mut CharacterIntent {
            movement: Vec2::X,
            ..Default::default()
        },
        0.25,
        &[],
    );
    assert!(!state.motion.grounded);
}

#[test]
fn current_support_controls_acceleration_not_previous_grounded_flag() {
    let mut world = tests::arena();
    let profile = MovementProfile {
        air_control: 0.0,
        ..Default::default()
    };
    let initial = actor(Vec3::new(0.5, -0.5, 0.5));
    let mut state = initial;
    let mut intent = CharacterIntent {
        movement: Vec2::X,
        ..Default::default()
    };
    tick(
        &world,
        &mut state,
        &CharacterBody::default(),
        &profile,
        &mut intent,
    );
    assert!(state.motion.position.x > initial.motion.position.x);
    state = initial;
    state.motion.grounded = true;
    // A single dug cell is only a shallow dip in smooth terrain; remove real
    // support under the actor.
    for x in -1..=1 {
        for z in -1..=1 {
            world.set_block(IVec3::new(x, -1, z), AIR);
        }
    }
    tick(
        &world,
        &mut state,
        &CharacterBody::default(),
        &profile,
        &mut intent,
    );
    assert_eq!(state.motion.position.x, initial.motion.position.x);
    assert!(!state.motion.grounded);
}

#[test]
fn large_actor_can_separate_from_a_deeply_embedded_cube() {
    let world = tests::arena();
    let body = CharacterBody::new(CollisionShape::new(1.0, 1.0, 3.0).unwrap(), 200.0).unwrap();
    let mut state = actor(Vec3::new(0.0, 0.0, 0.0));
    let cube = DynamicCollider::cube(1, Vec3::new(0.0, 1.5, 0.0), Vec3::ZERO);
    step_character(
        &world,
        &mut state,
        &body,
        &MovementProfile::default(),
        &mut CharacterIntent::default(),
        FIXED_DT,
        &[cube],
    );
    let (min, max) = physics::bounds(state.motion.position, body.shape);
    assert!(
        !(min.cmplt(cube.position + Vec3::splat(0.5 - EPSILON)).all()
            && max.cmpgt(cube.position - Vec3::splat(0.5 - EPSILON)).all())
    );
    assert!(state.motion.position.y >= -0.51);
}
