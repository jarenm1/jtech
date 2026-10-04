//! Blast jumps through the ordinary server movement, projectile and GPU schedules.
use super::*;
use protocol::BowPower;

fn ground_shot(gpu: bool, power: BowPower, pitch: f32) -> f32 {
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
    player.recent_charge = 60;
    player.state.motion.position = Vec3::new(16.5, 10.0, 16.5);
    player.state.motion.grounded = true;
    {
        let mut sim = app.world_mut().resource_mut::<Simulation>();
        sim.players.insert(1, player);
        sim.fire_bow(&world, 1, 1, 0.0, pitch, power);
        assert_eq!(sim.metrics.bow_shots, 1);
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
        "gpu={gpu} power={power:?} pitch={pitch} launch={launch_speed:.3} rise={:.3}",
        peak - 10.0
    );
    peak - 10.0
}

#[test]
fn ground_shots_have_visible_lift_through_server_ticks() {
    for (power, minimum_rise) in [
        (BowPower::Low, [1.3, 0.7, 0.08]),
        (BowPower::Standard, [3.5, 2.0, 0.4]),
        (BowPower::High, [8.0, 5.0, 1.0]),
        (BowPower::Extreme, [18.0, 11.0, 3.0]),
    ] {
        for (pitch, minimum) in [-1.54, -1.2, -0.8].into_iter().zip(minimum_rise) {
            let rise = ground_shot(false, power, pitch);
            assert!(
                rise > minimum,
                "{power:?} pitch={pitch}: rise={rise}, minimum={minimum}"
            );
        }
    }
}

#[test]
#[ignore = "requires a headless GPU adapter"]
fn ground_shots_have_visible_lift_with_gpu_debris() {
    let _guard = physics_slice::GPU_TEST_LOCK.lock().unwrap();
    for (power, minimum) in [(BowPower::Standard, 2.0), (BowPower::Extreme, 11.0)] {
        let rise = ground_shot(true, power, -1.2);
        assert!(rise > minimum, "{power:?}: rise={rise}, minimum={minimum}");
    }
}
