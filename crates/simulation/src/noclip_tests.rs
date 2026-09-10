use super::*;

#[test]
fn flight_inputs_replicate_and_missing_input_hovers_without_body_push() {
    let mut app = App::new();
    app.add_plugins(SimulationPlugin::headless(ServerConfig::default()).unwrap());
    let mut world = VoxelWorld::default();
    world.insert(IVec3::ZERO, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
    app.insert_resource(world);
    let mut player = Player::new();
    player.state.position = Vec3::new(4.0, 8.0, 4.0);
    let input = PlayerInput {
        sequence: 1,
        noclip: true,
        descend: true,
        ..Default::default()
    };
    let bytes = protocol::encode(&input, protocol::MAX_DATAGRAM).unwrap();
    player.enqueue(protocol::decode(&bytes, protocol::MAX_DATAGRAM).unwrap());
    app.world_mut()
        .resource_mut::<Simulation>()
        .players
        .insert(1, player);
    app.update();
    let sim = app.world().resource::<Simulation>();
    let player = &sim.players[&1];
    assert!(player.state.noclip);
    assert!(player.state.position.y < 8.0);
    assert_eq!(player.body_push_velocity, Vec3::ZERO);
    let position = player.state.position;
    let snapshot = player.snapshot(1);
    let bytes = protocol::encode(&snapshot, protocol::MAX_DATAGRAM).unwrap();
    let received: PlayerSnapshot = protocol::decode(&bytes, protocol::MAX_DATAGRAM).unwrap();
    assert!(received.state.noclip);
    for _ in 0..3 {
        app.update();
    }
    let sim = app.world().resource::<Simulation>();
    assert_eq!(sim.players[&1].state.position, position);
    assert_eq!(sim.players[&1].state.velocity, Vec3::ZERO);
    app.world_mut()
        .resource_mut::<Simulation>()
        .players
        .get_mut(&1)
        .unwrap()
        .enqueue(PlayerInput {
            sequence: 2,
            noclip: false,
            ..Default::default()
        });
    app.update();
    let state = app.world().resource::<Simulation>().players[&1].state;
    assert!(!state.noclip);
    assert!(state.velocity.y < 0.0);
}
