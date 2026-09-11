//! Bounded streaming work for large view radii.
use glam::IVec3;

pub(super) const CHUNKS_PER_TICK: usize = 8;

/// Select a small nearest batch in linear time, then sort only that batch.
pub(super) fn nearest_chunks<K: Ord>(
    mut chunks: Vec<IVec3>,
    limit: usize,
    mut priority: impl FnMut(&IVec3) -> K,
) -> Vec<IVec3> {
    if chunks.len() > limit {
        chunks.select_nth_unstable_by_key(limit, &mut priority);
        chunks.truncate(limit);
    }
    chunks.sort_unstable_by_key(priority);
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Player, ServerConfig, Simulation, SimulationPlugin, interests};
    use bevy_app::App;
    use glam::Vec3;
    use protocol::{DEFAULT_VIEW_RADIUS, MAX_VIEW_RADIUS};
    use voxel_world::VoxelWorld;

    #[test]
    fn large_interest_covers_edges_and_honors_configuration() {
        for radius in [DEFAULT_VIEW_RADIUS, MAX_VIEW_RADIUS] {
            let plugin = SimulationPlugin::headless(ServerConfig {
                radius,
                ..Default::default()
            })
            .unwrap();
            assert_eq!(plugin.config.radius, radius);
            let bounds = (-radius..=radius)
                .flat_map(|z| (-radius..=radius).map(move |x| (glam::IVec2::new(x, z), (20, 24))))
                .collect();
            let chunks = interests(Vec3::splat(0.5), radius, &bounds, std::iter::empty());
            assert!(chunks.len() < ((radius * 2 + 1).pow(2) * 4) as usize);
            assert!(chunks.contains(&IVec3::new(radius, 0, -radius)));
            assert!(!chunks.contains(&IVec3::new(radius + 1, 0, 0)));
        }
    }

    #[test]
    fn nearest_batch_preserves_priority_without_sorting_whole_interest() {
        let chunks: Vec<_> = (-64..=64).map(|x| IVec3::new(x, 0, 0)).collect();
        let key = |coord: &IVec3| (coord.x != 60, coord.length_squared(), coord.x);
        let mut expected = chunks.clone();
        expected.sort_unstable_by_key(key);
        expected.truncate(CHUNKS_PER_TICK);
        assert_eq!(
            nearest_chunks(chunks.clone(), CHUNKS_PER_TICK, key),
            expected
        );
        assert!(nearest_chunks(chunks, 0, key).is_empty());
        assert!(nearest_chunks(Vec::new(), CHUNKS_PER_TICK, key).is_empty());
    }

    #[test]
    #[ignore = "manual full-interest server streaming benchmark"]
    fn large_radius_streaming_benchmark() {
        for radius in [3, DEFAULT_VIEW_RADIUS, MAX_VIEW_RADIUS] {
            let mut app = App::new();
            app.add_plugins(
                SimulationPlugin::headless(ServerConfig {
                    radius,
                    metrics_every: 0,
                    ..Default::default()
                })
                .unwrap(),
            );
            let mut player = Player::new();
            player.state.position = app.world().resource::<Simulation>().spawn;
            player.state.noclip = true;
            player.input.noclip = true;
            app.world_mut()
                .resource_mut::<Simulation>()
                .players
                .insert(1, player);
            let started = std::time::Instant::now();
            let mut ticks = 0;
            let deadline = started + std::time::Duration::from_secs(300);
            loop {
                app.update();
                ticks += 1;
                let sim = app.world().resource::<Simulation>();
                let world = app.world().resource::<VoxelWorld>();
                let columns = ((radius * 2 + 5).pow(2)) as usize;
                if sim.terrain.bounds.len() == columns
                    && sim.players[&1]
                        .safety
                        .iter()
                        .all(|coord| world.chunks.contains_key(coord))
                    && sim.players[&1].known.len() == sim.players[&1].interest.len()
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "streaming did not complete"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            for _ in 0..120 {
                app.update();
            }
            let sim = app.world().resource::<Simulation>();
            let world = app.world().resource::<VoxelWorld>();
            assert_eq!(sim.players[&1].known.len(), sim.players[&1].interest.len());
            eprintln!(
                "radius={radius} fill_ticks={ticks} elapsed_s={:.3}",
                started.elapsed().as_secs_f32()
            );
            sim.print_metrics(world.chunks.len());
        }
    }
}
