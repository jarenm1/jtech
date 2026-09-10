use super::*;
use physics::step_player;
use protocol::PlayerSnapshot;
use voxel_world::{AIR, STONE};

fn arena() -> VoxelWorld {
    let mut world = VoxelWorld::default();
    for x in -1..=1 {
        for z in -1..=1 {
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

#[test]
fn reconciliation_copies_authoritative_health_without_predicting_damage() {
    let world = arena();
    let start = PlayerState {
        position: Vec3::new(0.5, 0.0, 0.5),
        velocity: Vec3::ZERO,
        grounded: true,
    };
    let mut client = ClientSession {
        state: start,
        ..default()
    };
    let mut health = Health::default();
    health.damage(35);
    let mut snapshot = Snapshot {
        tick: 1,
        you: PlayerSnapshot {
            id: 1,
            last_input: 0,
            state: start,
            health,
            yaw: 0.0,
        },
        players: vec![],
    };
    client.pending.push_back(PlayerInput {
        sequence: 1,
        movement: [1.0, 0.0],
        ..default()
    });
    reconcile(&mut client, &world, &snapshot, &[]);
    assert_eq!(client.health, health);
    assert!(client.state.position.x > start.position.x);

    snapshot.tick += 1;
    snapshot.you.health.damage(u16::MAX);
    reconcile(&mut client, &world, &snapshot, &[]);
    assert!(client.health.is_depleted());

    snapshot.tick += 1;
    snapshot.you.health.heal(20);
    reconcile(&mut client, &world, &snapshot, &[]);
    assert_eq!(client.health, snapshot.you.health);
}

#[test]
fn movement_prediction_preserves_replicated_health() {
    let world = arena();
    for damage in [35, 100] {
        let mut health = Health::default();
        health.damage(damage);
        let mut client = ClientSession {
            state: PlayerState {
                position: Vec3::new(0.5, 0.0, 0.5),
                velocity: Vec3::ZERO,
                grounded: true,
            },
            health,
            ..default()
        };
        let start = client.state;
        for sequence in 1..=20 {
            client.predict_input(
                &world,
                PlayerInput {
                    sequence,
                    movement: [1.0, 0.0],
                    ..default()
                },
                &[],
            );
        }
        assert_eq!(client.health, health);
        assert!(client.state.position.x > start.position.x);
        assert_eq!(client.pending.len(), 20);
    }
}

#[test]
fn health_hud_bar_tracks_authoritative_fraction() {
    let mut app = App::new();
    app.init_resource::<ClientSession>()
        .add_systems(Startup, |mut commands: Commands| {
            health_hud::spawn(&mut commands)
        })
        .add_systems(Update, health_hud::update);
    for (damage, expected_width) in [(0, 100.0), (35, 65.0), (65, 0.0)] {
        app.world_mut()
            .resource_mut::<ClientSession>()
            .health
            .damage(damage);
        app.update();
        let world = app.world_mut();
        let mut fills = world.query_filtered::<&Node, With<health_hud::HealthFill>>();
        assert_eq!(fills.single(world).unwrap().width, percent(expected_width));
    }
}

#[test]
fn reconciliation_replays_only_unacknowledged_inputs_and_keeps_smoothing_out_of_physics() {
    let world = arena();
    let start = PlayerState {
        position: Vec3::new(0.5, 0.0, 0.5),
        velocity: Vec3::ZERO,
        grounded: true,
    };
    let mut client = ClientSession {
        state: start,
        ..default()
    };
    let mut acknowledged = start;
    let mut expected = start;
    for sequence in 1..=30 {
        let input = PlayerInput {
            sequence,
            movement: [1.0, 0.0],
            ..default()
        };
        step_player(&world, &mut expected, &input, FIXED_DT);
        if sequence == 10 {
            acknowledged = expected;
        }
        client.pending.push_back(input);
    }
    client.state = expected;
    client.state.position.x += 1.0;
    let snapshot = Snapshot {
        tick: 10,
        you: PlayerSnapshot {
            id: 1,
            last_input: 10,
            state: acknowledged,
            health: Health::default(),
            yaw: 0.0,
        },
        players: vec![],
    };
    reconcile(&mut client, &world, &snapshot, &[]);
    assert_eq!(client.state, expected);
    assert_eq!(client.pending.front().unwrap().sequence, 11);
    assert_eq!(client.pending.back().unwrap().sequence, 30);
    assert!((client.correction.x - 1.0).abs() < 0.0001);
    // The next authoritative state acknowledges the rest. Replaying those again
    // would move the player too far; visual correction must not alter collision.
    reconcile(
        &mut client,
        &world,
        &Snapshot {
            tick: 30,
            you: PlayerSnapshot {
                id: 1,
                last_input: 30,
                state: expected,
                health: Health::default(),
                yaw: 0.0,
            },
            players: vec![],
        },
        &[],
    );
    assert!(client.pending.is_empty());
    assert_eq!(client.state, expected);
}

#[test]
fn terrain_changes_are_used_when_replaying_prediction() {
    let mut world = arena();
    let start = PlayerState {
        position: Vec3::new(0.5, 0.0, 0.5),
        velocity: Vec3::ZERO,
        grounded: true,
    };
    let mut client = ClientSession {
        state: start,
        ..default()
    };
    for sequence in 1..=20 {
        let input = PlayerInput {
            sequence,
            movement: [1.0, 0.0],
            ..default()
        };
        step_player(&world, &mut client.state, &input, FIXED_DT);
        client.pending.push_back(input);
    }
    assert!(client.state.position.x > 2.0);
    for y in 0..3 {
        world.set_block(IVec3::new(1, y, 0), STONE);
    }
    reconcile(
        &mut client,
        &world,
        &Snapshot {
            tick: 1,
            you: PlayerSnapshot {
                id: 1,
                last_input: 0,
                state: start,
                health: Health::default(),
                yaw: 0.0,
            },
            players: vec![],
        },
        &[],
    );
    assert!((client.state.position.x - 0.7).abs() < 0.0001);
    assert!(!physics::overlaps_block(&client.state, IVec3::new(1, 0, 0)));
}

#[test]
fn reconciliation_uses_authoritative_loose_block_colliders() {
    let world = arena();
    let start = PlayerState {
        position: Vec3::new(0.5, 0.0, 0.5),
        velocity: Vec3::ZERO,
        grounded: true,
    };
    let mut client = ClientSession {
        state: start,
        ..default()
    };
    for sequence in 1..=20 {
        let input = PlayerInput {
            sequence,
            movement: [1.0, 0.0],
            ..default()
        };
        step_player(&world, &mut client.state, &input, FIXED_DT);
        client.pending.push_back(input);
    }
    let body = physics::DynamicCollider {
        id: 1,
        position: Vec3::new(1.5, 0.5, 0.5),
        velocity: Vec3::ZERO,
    };
    reconcile(
        &mut client,
        &world,
        &Snapshot {
            tick: 1,
            you: PlayerSnapshot {
                id: 1,
                last_input: 0,
                state: start,
                health: Health::default(),
                yaw: 0.0,
            },
            players: vec![],
        },
        &[body],
    );
    assert!((client.state.position.x - 0.7).abs() < 0.001);
}

#[test]
fn action_feedback_tracks_partial_hits_destruction_and_rejections() {
    let mut client = ClientSession::default();
    for (request, damage) in [(1, 0.2), (2, 0.4), (3, 1.0)] {
        let result = ActionResult {
            request,
            accepted: true,
            reason: None,
            damage: Some(damage),
        };
        client.receive_action(result);
        assert_eq!(client.last_action, Some(result));
        assert_eq!(client.accepted_edits, request);
        assert_eq!(client.rejected_edits, 0);
        assert!(client.resyncing.is_empty());
    }
    let rejected = ActionResult {
        request: 4,
        accepted: false,
        reason: Some(EditRejection::Cooldown),
        damage: None,
    };
    client.receive_action(rejected);
    assert_eq!(client.last_action, Some(rejected));
    assert_eq!(client.accepted_edits, 3);
    assert_eq!(client.rejected_edits, 1);

    let placed = ActionResult {
        request: 5,
        accepted: true,
        reason: None,
        damage: None,
    };
    client.receive_action(placed);
    assert_eq!(client.last_action, Some(placed));
    assert_eq!(client.accepted_edits, 4);
}

#[test]
fn delayed_launch_result_does_not_replace_newer_hit_feedback() {
    let mut client = ClientSession::default();
    let hit = ActionResult {
        request: 2,
        accepted: true,
        reason: None,
        damage: Some(0.3),
    };
    client.receive_action(hit);
    client.receive_action(ActionResult {
        request: 1,
        accepted: false,
        reason: Some(EditRejection::Expired),
        damage: None,
    });
    assert_eq!(client.last_action, Some(hit));
    assert_eq!(client.accepted_edits, 1);
    assert_eq!(client.rejected_edits, 1);
}

#[test]
fn bow_slot_selection_and_untargeted_shot_routing() {
    let mut keys = ButtonInput::<KeyCode>::default();
    keys.press(KeyCode::Digit6);
    assert_eq!(selected_slot(&keys), Some(EXPLOSIVE_BOW_SLOT));
    let mut client = ClientSession {
        selected: selected_slot(&keys).unwrap(),
        yaw: 0.75,
        pitch: 1.0,
        ..default()
    };
    // Empty/unloaded terrain and sky aiming must not suppress bow shots.
    let world = VoxelWorld::default();
    assert!(matches!(
        block_action(&mut client, &world, false, false, true),
        Some(ClientMessage::FireBow {
            request: 1,
            yaw: 0.75,
            pitch: 1.0
        })
    ));
    assert!(block_action(&mut client, &world, false, false, false).is_none());
    assert!(block_action(&mut client, &world, false, true, false).is_none());
    assert_eq!(client.request, 1);
    keys.reset_all();
    keys.press(KeyCode::Digit3);
    client.selected = selected_slot(&keys).unwrap();
    assert!(block_action(&mut client, &world, false, false, true).is_none());
}

#[test]
fn equipped_bow_hits_and_debug_launches_use_grid_actions() {
    let mut client = ClientSession {
        selected: EXPLOSIVE_BOW_SLOT,
        pitch: -1.0,
        state: PlayerState {
            position: Vec3::new(0.5, 0.0, 0.5),
            ..default()
        },
        ..default()
    };
    let world = arena();
    assert!(matches!(
        block_action(&mut client, &world, false, true, true),
        Some(ClientMessage::Edit { block: 0, .. })
    ));
    assert!(matches!(
        block_action(&mut client, &world, true, false, true),
        Some(ClientMessage::Strike { .. })
    ));
    for selected in 1..=5 {
        client.selected = selected;
        assert!(
            matches!(block_action(&mut client, &world, false, false, true),
            Some(ClientMessage::Edit { block, .. }) if block == selected)
        );
    }
    client.selected = 7;
    assert!(block_action(&mut client, &world, false, false, true).is_none());
}
