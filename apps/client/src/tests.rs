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
