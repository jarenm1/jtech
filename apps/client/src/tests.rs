use super::*;
use controller::{CharacterState, step_player};
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
fn reconciliation_preserves_blast_momentum_through_input_replay() {
    let world = arena();
    let start = PlayerState {
        position: Vec3::new(0.5, 0.0, 0.5),
        grounded: true,
        ..default()
    };
    let mut client = ClientSession {
        state: start,
        ..default()
    };
    for sequence in 1..=10 {
        client.predict_input(
            &world,
            PlayerInput {
                sequence,
                ..default()
            },
            &[],
        );
    }
    let mut authoritative = start;
    physics::apply_player_impulse(&mut authoritative, Vec3::new(400.0, 640.0, 0.0));
    let snapshot = Snapshot {
        tick: 1,
        you: PlayerSnapshot {
            id: 1,
            last_input: 4,
            life: 0,
            state: CharacterState { motion: authoritative, ..Default::default() },
            health: Health::default(),
        },
        players: vec![],
        actors: vec![],
    };
    let mut expected = authoritative;
    for sequence in 5..=10 {
        step_player(
            &world,
            &mut expected,
            &PlayerInput {
                sequence,
                ..default()
            },
            FIXED_DT,
        );
    }
    reconcile(&mut client, &world, &snapshot, &[]);
    assert_eq!(client.state, expected);
    assert!(client.state.position.x > start.position.x);
    assert!(client.state.position.y > start.position.y);
    assert!(client.state.external_velocity.x > 0.0);
    reconcile(&mut client, &world, &snapshot, &[]);
    assert_eq!(client.state, expected);
}
#[test]
fn reconciliation_copies_authoritative_health_without_predicting_damage() {
    let world = arena();
    let start = PlayerState {
        position: Vec3::new(0.5, 0.0, 0.5),
        velocity: Vec3::ZERO,
        grounded: true,
        ..default()
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
            life: 0,
            state: CharacterState { motion: start, ..Default::default() },
            health,
        },
        players: vec![],
        actors: vec![],
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
                ..default()
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
        ..default()
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
            life: 0,
            state: CharacterState { motion: acknowledged, ..Default::default() },
            health: Health::default(),
        },
        players: vec![],
        actors: vec![],
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
                life: 0,
                state: CharacterState { motion: expected, ..Default::default() },
                health: Health::default(),
            },
            players: vec![],
        actors: vec![],
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
        ..default()
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
                life: 0,
                state: CharacterState { motion: start, ..Default::default() },
                health: Health::default(),
            },
            players: vec![],
        actors: vec![],
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
        ..default()
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
    let body = physics::DynamicCollider::cube(1, Vec3::new(1.5, 0.5, 0.5), Vec3::ZERO);
    reconcile(
        &mut client,
        &world,
        &Snapshot {
            tick: 1,
            you: PlayerSnapshot {
                id: 1,
                last_input: 0,
                life: 0,
                state: CharacterState { motion: start, ..Default::default() },
                health: Health::default(),
            },
            players: vec![],
        actors: vec![],
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
    assert_eq!(selected_slot(&keys), Some(6));
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
            pitch: 1.0,
            power: BowPower::Standard,
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
fn bow_requests_copy_each_selected_power() {
    let mut client = ClientSession {
        selected: 6,
        ..default()
    };
    let world = VoxelWorld::default();
    for (index, power) in [
        BowPower::Low,
        BowPower::Standard,
        BowPower::High,
        BowPower::Extreme,
    ]
    .into_iter()
    .enumerate()
    {
        client.bow_power = power;
        assert!(matches!(
            block_action(&mut client, &world, false, false, true),
            Some(ClientMessage::FireBow { power: sent, request, .. })
                if sent == power && request == index as u64 + 1
        ));
    }
}
#[test]
fn bow_power_keyboard_cycles_once_and_preserves_selection_across_slots() {
    let mut app = App::new();
    app.insert_resource(Options {
        server: "127.0.0.1:4000".parse().unwrap(),
        ..default()
    })
    .insert_resource(ClientSession {
        selected: 6,
        ..default()
    })
    .init_resource::<ButtonInput<KeyCode>>()
    .init_resource::<AccumulatedMouseMotion>()
    .init_resource::<pause_menu::PauseMenu>()
    .add_systems(Update, controls);
    let cursor = app
        .world_mut()
        .spawn(CursorOptions {
            visible: false,
            grab_mode: CursorGrabMode::Locked,
            ..default()
        })
        .id();
    assert_eq!(
        app.world().resource::<ClientSession>().bow_power,
        BowPower::Standard
    );
    for expected in [
        BowPower::High,
        BowPower::Extreme,
        BowPower::Low,
        BowPower::Standard,
    ] {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyR);
        app.update();
        assert_eq!(app.world().resource::<ClientSession>().bow_power, expected);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        app.update();
        assert_eq!(app.world().resource::<ClientSession>().bow_power, expected);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset_all();
    }
    for key in [KeyCode::Digit3, KeyCode::Digit6] {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(key);
        app.update();
        assert_eq!(
            app.world().resource::<ClientSession>().bow_power,
            BowPower::Standard
        );
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset_all();
    }
    app.world_mut()
        .entity_mut(cursor)
        .get_mut::<CursorOptions>()
        .unwrap()
        .visible = true;
    app.world_mut()
        .resource_mut::<ButtonInput<KeyCode>>()
        .press(KeyCode::KeyR);
    app.update();
    assert_eq!(
        app.world().resource::<ClientSession>().bow_power,
        BowPower::Standard
    );
    app.world_mut()
        .entity_mut(cursor)
        .get_mut::<CursorOptions>()
        .unwrap()
        .visible = false;
    app.world_mut().resource_mut::<ClientSession>().selected = 3;
    app.update();
    assert_eq!(
        app.world().resource::<ClientSession>().bow_power,
        BowPower::Standard
    );
}
#[test]
fn equipped_bow_hits_and_debug_launches_use_grid_actions() {
    let mut client = ClientSession {
        selected: 6,
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
    // Material and empty slots never send a secondary action.
    for selected in [1, 5, 7, 8, 9, 10] {
        client.selected = selected;
        assert!(block_action(&mut client, &world, false, false, true).is_none());
    }
}
#[test]
fn bow_cadence_changes_immediately_and_zero_rate_disables_firing() {
    let mut repeat = BowRepeat::default();
    assert!(repeat_bow(&mut repeat, 0.0, true, 1));
    assert!(repeat_bow(&mut repeat, 0.1, true, 10));
    assert!(!repeat_bow(&mut repeat, 0.11, true, 10));
    assert!(repeat_bow(&mut repeat, 0.2, true, 10));
    assert!(!repeat_bow(&mut repeat, 0.3, true, 0));
    assert_eq!(repeat.next, None);
    assert!(!repeat_bow(&mut repeat, 1.0, true, 0));
    assert!(repeat_bow(&mut repeat, 1.1, true, 25));
}
#[test]
fn reconciliation_replays_flight_mode_and_returns_to_walking() {
    let world = arena();
    let mut client = ClientSession {
        state: PlayerState {
            position: Vec3::new(0.5, 4.0, 0.5),
            ..default()
        },
        noclip_requested: true,
        ..default()
    };
    let mut server = client.state;
    let mut acknowledged = server;
    for sequence in 1..=10 {
        let input = PlayerInput {
            sequence,
            noclip: true,
            jump: true,
            ..default()
        };
        client.predict_input(&world, input, &[]);
        step_player(&world, &mut server, &input, FIXED_DT);
        if sequence == 5 {
            acknowledged = server;
        }
    }
    let mut snapshot = Snapshot {
        tick: 5,
        you: PlayerSnapshot {
            id: 1,
            last_input: 5,
            life: 0,
            state: CharacterState { motion: acknowledged, ..Default::default() },
            health: Health::default(),
        },
        players: vec![],
        actors: vec![],
    };
    reconcile(&mut client, &world, &snapshot, &[]);
    assert_eq!(client.state, server);
    assert!(client.state.noclip);
    assert!(client.noclip_requested);
    client.noclip_requested = false;
    let input = PlayerInput {
        sequence: 11,
        noclip: false,
        ..default()
    };
    client.predict_input(&world, input, &[]);
    step_player(&world, &mut server, &input, FIXED_DT);
    // Replaying an older flying snapshot includes the later exit command.
    snapshot.tick = 10;
    snapshot.you.last_input = 10;
    snapshot.you.state.motion = acknowledged;
    for sequence in 6..=10 {
        step_player(
            &world,
            &mut snapshot.you.state.motion,
            &PlayerInput {
                sequence,
                noclip: true,
                jump: true,
                ..default()
            },
            FIXED_DT,
        );
    }
    reconcile(&mut client, &world, &snapshot, &[]);
    assert_eq!(client.state, server);
    assert!(!client.state.noclip);
    assert!(client.state.velocity.y < 0.0);
}
#[test]
fn camera_far_distance_covers_vertical_corner_of_view_region() {
    // The interest square reaches (MAX_VIEW_RADIUS + 2) chunks along each
    // horizontal axis, so a corner block is offset by that margin on both.
    let margin = (protocol::MAX_VIEW_RADIUS + 2) as f32 * CHUNK_SIZE as f32;
    let far = camera_far_distance();
    // Camera at the top of the world, looking toward the opposite bottom corner.
    let vertical = (WORLD_MAX_Y - WORLD_MIN_Y + 1) as f32 + EYE_HEIGHT;
    let corner = Vec3::new(margin, vertical, margin);
    assert!(
        far >= corner.length(),
        "far plane {far} clips corner {corner}"
    );
}
fn snapshot(tick: u64, life: u64, state: PlayerState, health: Health) -> Snapshot {
    Snapshot {
        tick,
        you: PlayerSnapshot {
            id: 1,
            last_input: 0,
            state: CharacterState { motion: state, ..Default::default() },
            health,
            life,
        },
        players: vec![],
        actors: vec![],
    }
}
#[test]
fn reordered_snapshots_ignore_stale_life_and_state() {
    let world = arena();
    let start = PlayerState {
        position: Vec3::new(0.5, 0.0, 0.5),
        grounded: true,
        ..default()
    };
    let mut client = ClientSession {
        state: start,
        ..default()
    };
    let current = PlayerState {
        position: Vec3::new(4.5, 0.0, 0.5),
        ..default()
    };
    reconcile(
        &mut client,
        &world,
        &snapshot(5, 1, current, Health::default()),
        &[],
    );
    assert_eq!(client.life, 1);
    assert_eq!(client.state, current);
    // A late datagram from before the respawn must not rewind life or motion.
    let stale = PlayerState {
        position: Vec3::new(9.5, 0.0, 0.5),
        ..default()
    };
    reconcile(
        &mut client,
        &world,
        &snapshot(4, 7, stale, Health::default()),
        &[],
    );
    assert_eq!(client.life, 1);
    assert_eq!(client.state, current);
    assert_eq!(client.last_tick, 5);
}
#[test]
fn life_change_resets_replay_presentation_noclip_and_sequence() {
    let world = arena();
    let start = PlayerState {
        position: Vec3::new(0.5, 0.0, 0.5),
        grounded: true,
        ..default()
    };
    let mut client = ClientSession {
        state: start,
        sequence: 12,
        ..default()
    };
    client.pending.push_back(PlayerInput {
        sequence: 12,
        movement: [1.0, 0.0],
        ..default()
    });
    client.correction = Vec3::new(1.0, 0.0, 0.0);
    client.noclip_requested = true;
    let respawned = PlayerState {
        position: Vec3::new(30.5, 4.0, -8.5),
        velocity: Vec3::new(2.0, 0.0, 0.0),
        noclip: false,
        ..default()
    };
    reconcile(
        &mut client,
        &world,
        &snapshot(9, 1, respawned, Health::default()),
        &[],
    );
    assert_eq!(client.life, 1);
    assert_eq!(client.state, respawned);
    assert!(client.pending.is_empty());
    assert_eq!(client.correction, Vec3::ZERO);
    assert!(!client.noclip_requested);
    assert_eq!(client.sequence, 12, "sequence stays monotonic");
}
#[test]
fn life_change_applies_even_when_the_snapshot_is_still_dead() {
    let world = arena();
    let mut client = ClientSession {
        correction: Vec3::new(0.5, 0.0, 0.0),
        noclip_requested: true,
        ..default()
    };
    client.pending.push_back(PlayerInput {
        sequence: 3,
        movement: [1.0, 0.0],
        ..default()
    });
    let mut health = Health::default();
    health.damage(u16::MAX);
    let respawned = PlayerState {
        position: Vec3::new(2.5, 0.0, 0.5),
        noclip: false,
        ..default()
    };
    reconcile(&mut client, &world, &snapshot(2, 1, respawned, health), &[]);
    assert_eq!(client.life, 1);
    assert_eq!(client.state, respawned);
    assert!(client.pending.is_empty());
    assert_eq!(client.correction, Vec3::ZERO);
    assert!(!client.noclip_requested);
    assert!(client.health.is_depleted());
}
#[test]
fn dead_reconciliation_discards_local_replay_and_keeps_sequence_monotonic() {
    let world = arena();
    let mut client = ClientSession {
        sequence: 40,
        ..default()
    };
    for sequence in [39, 40] {
        client.pending.push_back(PlayerInput {
            sequence,
            movement: [1.0, 0.0],
            ..default()
        });
    }
    client.correction = Vec3::new(1.0, 0.0, 0.0);
    let mut health = Health::default();
    health.damage(u16::MAX);
    let dead_state = PlayerState {
        position: Vec3::new(0.5, 0.0, 0.5),
        ..default()
    };
    reconcile(
        &mut client,
        &world,
        &snapshot(3, 0, dead_state, health),
        &[],
    );
    assert!(client.health.is_depleted());
    assert_eq!(client.state, dead_state);
    assert!(client.pending.is_empty());
    assert_eq!(client.correction, Vec3::ZERO);
    assert_eq!(client.sequence, 40);
}
#[test]
fn input_packets_and_respawn_requests_carry_observed_life() {
    let client = ClientSession {
        session: 7,
        life: 5,
        ..default()
    };
    let input = PlayerInput {
        sequence: 1,
        movement: [0.0, 1.0],
        ..default()
    };
    let packet = client.input_packet(vec![input].into());
    assert_eq!(packet.session, 7);
    assert_eq!(packet.life, 5);
    assert_eq!(packet.inputs.len(), 1);
    assert_eq!(packet.inputs[0].sequence, 1);
    assert_eq!(packet.inputs[0].movement, [0.0, 1.0]);
    assert!(matches!(
        client.respawn_request(),
        ClientMessage::Respawn { life: 5 }
    ));
    assert!(matches!(
        client.respawn_request(),
        ClientMessage::Respawn { life: 5 }
    ));
}
#[test]
fn controls_block_look_and_slot_changes_while_dead() {
    let mut app = App::new();
    app.insert_resource(Options {
        server: "127.0.0.1:4000".parse().unwrap(),
        ..default()
    })
    .insert_resource(ClientSession {
        selected: 3,
        ..default()
    })
    .init_resource::<ButtonInput<KeyCode>>()
    .init_resource::<AccumulatedMouseMotion>()
    .init_resource::<pause_menu::PauseMenu>()
    .add_systems(Update, controls);
    app.world_mut().spawn(CursorOptions {
        visible: false,
        grab_mode: CursorGrabMode::Locked,
        ..default()
    });
    let mut health = Health::default();
    health.damage(u16::MAX);
    app.world_mut().resource_mut::<ClientSession>().health = health;
    let yaw = app.world().resource::<ClientSession>().yaw;
    app.world_mut()
        .resource_mut::<ButtonInput<KeyCode>>()
        .press(KeyCode::Digit6);
    app.world_mut()
        .resource_mut::<AccumulatedMouseMotion>()
        .delta = Vec2::splat(50.0);
    app.update();
    assert_eq!(app.world().resource::<ClientSession>().selected, 3);
    assert_eq!(app.world().resource::<ClientSession>().yaw, yaw);
    app.world_mut().resource_mut::<ClientSession>().health = Health::default();
    app.update();
    assert_eq!(app.world().resource::<ClientSession>().selected, 6);
    assert_ne!(app.world().resource::<ClientSession>().yaw, yaw);
}
#[test]
fn death_heartbeat_keeps_sending_without_prediction_or_sequence_growth() {
    let mut client = ClientSession {
        life: 3,
        sequence: 40,
        ..default()
    };
    client.health.damage(100);
    let state = client.state;
    let mut packets = 0;
    for _ in 0..600 {
        if let Some(packet) = client.death_heartbeat(0.1) {
            packets += 1;
            assert_eq!(packet.life, 3);
            assert_eq!(packet.inputs.len(), 1);
            assert_eq!(packet.inputs[0].sequence, 40);
            assert_eq!(packet.inputs[0].movement, [0.0; 2]);
            assert!(!packet.inputs[0].jump && !packet.inputs[0].noclip);
        }
    }
    assert_eq!(packets, 120);
    assert!(client.pending.is_empty());
    assert_eq!(client.sequence, 40);
    assert_eq!(client.state, state);
}
#[test]
fn ground_jump_edge_survives_render_only_frames_and_fires_once_per_catchup() {
    let mut session = ClientSession::default();
    session.capture_jump(true, true);
    session.capture_jump(true, false); // Another rendered frame, no simulation tick yet.
    assert!(session.consume_jump(false, true));
    assert!(!session.consume_jump(false, true)); // Holding across catch-up ticks.
    session.capture_jump(true, true);
    session.capture_jump(false, false); // Pause cancels pending gameplay input.
    assert!(!session.consume_jump(false, false));
    assert!(session.consume_jump(true, true)); // Flight uses held ascent instead.
    assert!(session.consume_jump(true, true));
    assert!(!session.consume_jump(true, false));
}
#[test]
fn left_click_plays_swing_animation_without_a_melee_target() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut app = App::new();
    app.init_resource::<ButtonInput<MouseButton>>()
        .init_resource::<ButtonInput<KeyCode>>()
        .init_resource::<pause_menu::PauseMenu>()
        .init_resource::<RemoteActors>()
        .init_resource::<RemotePlayers>()
        .insert_resource(arena())
        .insert_resource(Time::<()>::default())
        .insert_resource(ClientSession {
            transport: Some(
                ClientTransport::connect(listener.local_addr().unwrap()).unwrap(),
            ),
            id: Some(1),
            ..default()
        })
        .add_systems(Update, edit_blocks);
    app.world_mut().spawn(CursorOptions {
        visible: false,
        grab_mode: CursorGrabMode::Locked,
        ..default()
    });
    app.update();
    assert!(app.world().resource::<ClientSession>().swing_at.is_none());
    app.world_mut()
        .resource_mut::<ButtonInput<MouseButton>>()
        .press(MouseButton::Left);
    app.update();
    let session = app.world().resource::<ClientSession>();
    assert!(session.swing_at.is_some());
    // No actor or remote player under the crosshair, so nothing attacks.
    assert!(!session.attack_pending);
}
