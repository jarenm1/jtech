//! Blast jumps through the ordinary server movement, projectile and GPU schedules.
use super::*;

fn ground_shot(gpu: bool, pitch: f32) -> f32 {
    let mut app = App::new();
    app.add_plugins(
        SimulationPlugin::headless(ServerConfig {
            gpu_physics: gpu,
            metrics_every: 0,
            ..Default::default()
        })
        .unwrap(),
    );
    let mut world = VoxelWorld::default();
    world.insert(IVec3::ZERO, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
    for x in 1..31 {
        for z in 1..31 {
            for y in 7..10 {
                world.set_block(IVec3::new(x, y, z), 3).unwrap();
            }
        }
    }
    let mut player = Player::new();
    player.state.motion.position = Vec3::new(16.5, 10.0, 16.5);
    player.state.motion.grounded = true;
    player.input.selected = protocol::EXPLOSIVE_BOW_ITEM;
    player.input.attack = true;
    player.input.yaw = 0.0;
    player.input.pitch = pitch;
    {
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        sim.players.insert(1, player);
        // The explosive bow is an admin item: it fires on the press edge, and
        // `resolve_attacks` spawns the package's standard shot.
        let attack = crate::basic_attack_for(&sim.packages, protocol::EXPLOSIVE_BOW_ITEM);
        let player = sim.players.get_mut(&1).unwrap();
        let pressed = controller::step_character_player(
            &world,
            &mut player.state,
            Some(attack),
            &player.input,
            physics::FIXED_DT,
            &[],
        );
        assert!(pressed.basic_attack, "the press edge must fire");
        // Release so the tick loop does not fire again.
        player.input.attack = false;
        controller::step_character_player(
            &world,
            &mut player.state,
            Some(attack),
            &player.input,
            physics::FIXED_DT,
            &[],
        );
        sim.resolve_attacks(&world, &[1]);
        assert_eq!(sim.arrows.len(), 1);
    }
    app.insert_resource(world);
    let mut peak = 10.0_f32;
    let mut launch_speed = 0.0_f32;
    for _ in 0..90 {
        app.update();
        let sim = app.world().resource::<Simulation>();
        let state = sim.players[&1].state;
        peak = peak.max(state.motion.position.y);
        launch_speed = launch_speed.max(state.motion.velocity.y);
        if gpu {
            app.world_mut()
                .resource_scope(|world, mut sim: Mut<Simulation>| {
                    let deadline = Instant::now() + std::time::Duration::from_secs(10);
                    while sim.physics.as_ref().unwrap().is_busy() {
                        sim.observe_physics(&mut world.resource_mut::<VoxelWorld>());
                        assert!(Instant::now() < deadline, "GPU completion timed out");
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                });
        }
    }
    let sim = app.world().resource::<Simulation>();
    assert_eq!(sim.metrics.explosions, 1);
    if gpu {
        assert!(!sim.physics.as_ref().unwrap().failed());
    }
    eprintln!(
        "gpu={gpu} pitch={pitch} launch={launch_speed:.3} rise={:.3}",
        peak - 10.0
    );
    peak - 10.0
}

#[test]
fn ground_shots_have_visible_lift_through_server_ticks() {
    // The explosive bow always fires its package's standard shot, so only the
    // aim pitch varies.
    for (pitch, minimum) in [-1.54, -1.2, -0.8].into_iter().zip([3.5, 2.0, 0.4]) {
        let rise = ground_shot(false, pitch);
        assert!(rise > minimum, "pitch={pitch}: rise={rise}, minimum={minimum}");
    }
}

#[test]
#[ignore = "requires a headless GPU adapter"]
fn ground_shots_have_visible_lift_with_gpu_debris() {
    let _guard = physics_slice::GPU_TEST_LOCK.lock().unwrap();
    let rise = ground_shot(true, -1.2);
    assert!(rise > 2.0, "rise={rise}");
}
