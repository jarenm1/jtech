//! Authoritative blast/lifecycle integration without a network transport.
use super::*;
use std::time::{Duration, Instant};
use voxel_world::Chunk;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(
        SimulationPlugin::headless(ServerConfig {
            gpu_physics: true,
            metrics_every: 0,
            ..Default::default()
        })
        .unwrap(),
    );
    app
}

fn with_sim<R>(app: &mut App, f: impl FnOnce(&mut Simulation, &mut VoxelWorld) -> R) -> R {
    let mut sim = app.world_mut().remove_resource::<Simulation>().unwrap();
    let result = f(&mut sim, &mut app.world_mut().resource_mut::<VoxelWorld>());
    app.insert_resource(sim);
    result
}

fn air(world: &mut VoxelWorld, coord: IVec3) {
    if !world.chunks.contains_key(&coord) {
        world.insert(coord, Chunk::from_runs(0, &[(32768, 0)]).unwrap());
    }
}

fn load_halos(sim: &Simulation, world: &mut VoxelWorld) {
    for coord in sim.physics.as_ref().unwrap().needed_chunks() {
        air(world, coord);
    }
}

fn complete(sim: &mut Simulation, world: &mut VoxelWorld) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while sim.physics.as_ref().unwrap().is_busy() {
        sim.observe_physics(world);
        assert!(Instant::now() < deadline, "GPU completion timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!sim.physics.as_ref().unwrap().failed());
}

fn submit(sim: &mut Simulation, world: &mut VoxelWorld) {
    load_halos(sim, world);
    for _ in 0..3 {
        sim.advance_physics(world);
        if sim.physics.as_ref().unwrap().is_busy() {
            return;
        }
    }
    panic!("loaded moving bodies did not submit a GPU batch");
}

#[test]
#[ignore = "requires a headless GPU adapter"]
fn simultaneous_remote_blasts_release_and_move_supported_terrain() {
    let _guard = physics_slice::GPU_TEST_LOCK.lock().unwrap();
    let mut app = app();
    with_sim(&mut app, |sim, world| {
        let regions = [IVec3::new(512, 0, 512), IVec3::new(-544, 0, -544)];
        for (id, base) in regions.iter().enumerate() {
            air(world, chunk_coord(*base));
            for y in 8..=9 {
                for z in 1..=11 {
                    for x in 1..=11 {
                        world.set_block(*base + IVec3::new(x, y, z), 3).unwrap();
                    }
                }
            }
            sim.detonations.push_back((
                id as u32,
                base.as_vec3() + Vec3::new(6.5, 10.02, 6.5),
                crate::packages::test_blast(protocol::BowPower::Standard),
            ));
        }
        sim.advance_bow(world);
        assert_eq!(sim.metrics.explosions, 2);
        let initial = sim.physics.as_ref().unwrap().snapshots();
        // Each detonation carves one crater and launches one debris body.
        assert_eq!(initial.len(), 2);
        for base in regions {
            assert!(
                initial
                    .iter()
                    .any(|b| (b.position.x - base.x as f32).abs() < 20.0)
            );
            // The crater is smooth: some carved cell keeps partial density.
            assert!(
                (1..=11).any(|x| {
                    (1..=11).any(|z| {
                        let cell = base + IVec3::new(x, 9, z);
                        world.density(cell).is_some_and(|d| d < 127 && d > -128)
                    })
                }),
                "blast left no partially carved voxel"
            );
        }
        for _ in 0..5 {
            submit(sim, world);
            complete(sim, world);
        }
        let physics = sim.physics.as_ref().unwrap();
        assert!(physics.resident_chunks() >= 8);
        for base in regions {
            let moved = physics
                .snapshots()
                .iter()
                .filter(|body| {
                    (body.position.x - base.x as f32).abs() < 20.0
                        && initial.iter().any(|start| {
                            start.id == body.id && body.position.distance(start.position) > 0.1
                        })
                })
                .count();
            assert_eq!(moved, 1, "remote region {base:?}: debris body did not move");
        }
    });
}

#[test]
#[ignore = "requires a headless GPU adapter"]
fn bombardment_retries_busy_bodies_and_preserves_impulses_through_compaction() {
    let _guard = physics_slice::GPU_TEST_LOCK.lock().unwrap();
    let mut app = app();
    with_sim(&mut app, |sim, world| {
        let cells = [IVec3::new(516, 24, 516), IVec3::new(532, 24, 516)];
        for (id, cell) in cells.iter().enumerate() {
            sim.detonations.push_back((
                id as u32,
                cell.as_vec3() + Vec3::splat(0.5) - Vec3::X * 1.0,
                crate::packages::test_blast(protocol::BowPower::Standard),
            ));
        }
        sim.advance_bow(world);
        let initial = sim.physics.as_ref().unwrap().snapshots();
        // One debris body per detonation, spawned undamaged.
        assert_eq!(initial.len(), 2);
        let doomed = initial[0].id;
        let survivor = initial[1].id;
        let initial_damage = sim.physics.as_ref().unwrap().body_damage(survivor);
        assert_eq!(initial_damage, 0.0);
        assert!(cells.iter().all(|cell| world.block(*cell) == Some(0)));
        assert!(cells.iter().all(|cell| !sim.damage.contains_key(cell)));
        // The impact arrives during GPU ownership. It must not mutate the old observation.
        sim.detonations.push_back((
            2,
            initial[1].position + Vec3::Z * 2.75,
            crate::packages::test_blast(protocol::BowPower::Standard),
        ));
        sim.advance_bow(world);
        assert_eq!(sim.detonations.len(), 1);
        assert_eq!(sim.metrics.explosions, 2);
        assert_eq!(
            sim.physics.as_ref().unwrap().body_damage(survivor),
            initial_damage
        );
        complete(sim, world);
        let moving = sim.physics.as_ref().unwrap().snapshots();
        assert!(moving.iter().all(|body| body.velocity.length() > 0.1));
        sim.advance_bow(world);
        assert!(sim.detonations.is_empty());
        let damaged = sim.physics.as_ref().unwrap().body_damage(survivor);
        assert!(damaged > initial_damage);

        // Before submitting that surviving body's impulse, remove the earlier slot.
        let victim = moving.iter().find(|body| body.id == doomed).unwrap();
        sim.detonations.push_back((
            3,
            victim.position,
            crate::packages::test_blast(protocol::BowPower::Standard),
        ));
        sim.advance_bow(world);
        assert_eq!(sim.metrics.destroyed_blocks, 1);
        let remaining = sim.physics.as_ref().unwrap().snapshots();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, survivor);
        submit(sim, world);
        complete(sim, world);
        let physics = sim.physics.as_ref().unwrap();
        let body = physics.snapshots()[0];
        assert_eq!(body.id, survivor);
        assert!(
            body.velocity.z < -0.5,
            "compaction lost the pending blast impulse: {body:?}"
        );
        assert!(physics.body_damage(survivor) >= damaged);

        // A fresh airborne observation is destructible, and cannot be destroyed twice.
        sim.detonations.push_back((
            4,
            body.position,
            crate::packages::test_blast(protocol::BowPower::Standard),
        ));
        sim.detonations.push_back((
            5,
            body.position,
            crate::packages::test_blast(protocol::BowPower::Standard),
        ));
        sim.advance_bow(world);
        assert!(sim.physics.as_ref().unwrap().snapshots().is_empty());
        assert_eq!(sim.metrics.destroyed_blocks, 2);
        sim.observe_physics(world);
        sim.advance_physics(world);
        assert_eq!(sim.metrics.destroyed_blocks, 2);
        assert_eq!(sim.metrics.explosions, 6);
    });
}

#[test]
#[ignore = "requires a headless GPU adapter"]
fn playerless_streaming_loads_moving_halos_and_retires_remote_regions() {
    let _guard = physics_slice::GPU_TEST_LOCK.lock().unwrap();
    let mut app = app();
    let source = IVec3::new(543, 45, 512);
    with_sim(&mut app, |sim, world| {
        air(world, chunk_coord(source));
        // Author a landing pad and wall in the next horizontal chunk. Other
        // halo chunks are intentionally absent so normal streaming must load them.
        for x in 540..=550 {
            for z in 510..=514 {
                let cell = IVec3::new(x, 40, z);
                air(world, chunk_coord(cell));
                world.set_block(cell, 3).unwrap();
            }
        }
        for y in 41..=48 {
            for z in 510..=514 {
                let cell = IVec3::new(546, y, z);
                air(world, chunk_coord(cell));
                world.set_block(cell, 3).unwrap();
            }
        }
        sim.physics
            .as_mut()
            .unwrap()
            .release(source, 3, 0.0, Vec3::X * 12.0);
        assert_eq!(sim.players.len(), 0);
        assert!(
            sim.physics
                .as_ref()
                .unwrap()
                .needed_chunks()
                .iter()
                .any(|coord| !world.chunks.contains_key(coord))
        );
    });
    // Terrain halos stream asynchronously, and the GPU will not step a body until every
    // collision page is resident. Wait for the normal stream to load them before starting
    // the crossing budget; the body stays frozen at the source cell until then.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        app.update();
        let resident = with_sim(&mut app, |sim, world| {
            sim.physics
                .as_ref()
                .unwrap()
                .needed_chunks()
                .iter()
                .all(|coord| world.chunks.contains_key(coord))
        });
        if resident {
            break;
        }
        assert!(Instant::now() < deadline, "terrain halos did not stream");
    }
    let mut crossed = false;
    for _ in 0..45 {
        app.update();
        with_sim(&mut app, |sim, world| {
            complete(sim, world);
            let physics = sim.physics.as_ref().unwrap();
            if physics.completions > 0 {
                assert!(
                    physics
                        .needed_chunks()
                        .iter()
                        .all(|coord| world.chunks.contains_key(coord))
                );
                crossed |= physics
                    .snapshots()
                    .iter()
                    .any(|body| body.position.x > 544.5);
                assert!(
                    physics
                        .snapshots()
                        .iter()
                        .all(|body| body.position.x < 546.0),
                    "body crossed the resident wall"
                );
            }
        });
    }
    assert!(crossed, "body failed to cross the remote chunk boundary");
    let remote = with_sim(&mut app, |sim, world| {
        let physics = sim.physics.as_mut().unwrap();
        let remote = physics.needed_chunks();
        assert!(physics.resident_chunks() > 0);
        assert!(
            physics
                .snapshots()
                .iter()
                .all(|body| body.velocity.x.abs() < 0.5),
            "the remote wall did not stop the body"
        );
        assert!(remote.iter().all(|coord| world.chunks.contains_key(coord)));
        remote
    });
    for _ in 0..300 {
        app.update();
        let settled = with_sim(&mut app, |sim, world| {
            complete(sim, world);
            sim.physics.as_ref().unwrap().snapshots().is_empty()
        });
        if settled {
            break;
        }
    }
    with_sim(&mut app, |sim, _| {
        assert!(
            sim.physics.as_ref().unwrap().snapshots().is_empty(),
            "body did not settle on its remote landing pad"
        );
        assert_eq!(sim.metrics.destroyed_blocks, 0);
        assert!(
            sim.journal
                .values()
                .any(|chunk| chunk.voxels.values().any(|voxel| voxel.material == 3)),
            "settled body was not restored to terrain"
        );
    });
    for _ in 0..123 {
        app.update();
        with_sim(&mut app, complete);
    }
    with_sim(&mut app, |sim, world| {
        assert!(sim.physics.as_ref().unwrap().needed_chunks().is_empty());
        assert_eq!(sim.physics.as_ref().unwrap().resident_chunks(), 0);
        assert!(remote.iter().all(|coord| !world.chunks.contains_key(coord)));
        assert!(
            remote
                .iter()
                .all(|coord| !sim.last_needed.contains_key(coord))
        );
    });
}
