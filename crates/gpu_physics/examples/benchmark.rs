//! cargo run --release -p gpu_physics --example benchmark
use gpu_physics::{Body, GpuPhysics, PlayerCollider, TERRAIN_EVENT_READBACK_BYTES, Terrain};
use std::time::{Duration, Instant};
fn percentile(values: &mut [Duration], p: f32) -> f64 {
    values.sort();
    values[((values.len() - 1) as f32 * p) as usize].as_secs_f64() * 1000.
}
fn main() -> Result<(), String> {
    let mut terrain = Terrain {
        origin: [0; 3],
        size: [64, 32, 64],
        cells: vec![0; 64 * 32 * 64],
    };
    for z in 0..64 {
        for x in 0..64 {
            let i = terrain.index(x, 0, z);
            terrain.cells[i] = 3;
        }
    }
    let start = Instant::now();
    let mut gpu = GpuPhysics::new(terrain)?;
    println!(
        "adapter={} init_ms={:.2}",
        gpu.adapter_name,
        start.elapsed().as_secs_f64() * 1000.
    );
    println!(
        "dt=1/120 per substep; resident state; 48 bytes/body plus bounded terrain events; server-batch cases use six substeps per readback (no networking)"
    );
    println!(
        "workload,bodies,substeps,submit_p50_ms,submit_p95_ms,wait_readback_p50_ms,batch_p50_ms,batch_p95_ms,batch_p99_ms,readback_payload_bytes"
    );
    for (workload, count, substeps, pile, player_count) in [
        ("backend-sparse", 32, 2, false, 0),
        ("backend-sparse", 128, 2, false, 0),
        ("backend-sparse", 512, 2, false, 0),
        ("backend-sparse", 1024, 2, false, 0),
        ("server-batch-sparse", 128, 6, false, 0),
        ("server-batch-pile", 128, 6, true, 0),
        ("server-batch-pile-16-players", 128, 6, true, 16),
    ] {
        let bodies: Vec<_> = (0..count)
            .map(|i| {
                Body::new(
                    if pile {
                        [
                            10.5 + (i % 8) as f32 * 1.01,
                            8.5 + (i / 32) as f32 * 1.01,
                            10.5 + ((i / 8) % 4) as f32 * 1.01,
                        ]
                    } else {
                        [
                            2.5 + (i % 28) as f32 * 2.,
                            8.5 + (i / 784) as f32 * 2.,
                            2.5 + ((i / 28) % 28) as f32 * 2.,
                        ]
                    },
                    3,
                )
            })
            .collect();
        let players: Vec<_> = (0..player_count)
            .map(|i| PlayerCollider {
                position: [10.2 + (i % 8) as f32, 1., 10.5 + (i / 8) as f32 * 2.],
                velocity: [1., 0., 0.],
                id: i,
                padding: 0,
            })
            .collect();
        gpu.set_players(&players)?;
        gpu.set_bodies(&bodies)?;
        for _ in 0..10 {
            gpu.submit(1. / 120., substeps)?;
            gpu.wait_readback()?;
            gpu.take_terrain_contacts();
        }
        let mut submit = vec![];
        let mut wait = vec![];
        let mut tick = vec![];
        for _ in 0..120 {
            let start = Instant::now();
            gpu.set_players(&players)?;
            gpu.submit(1. / 120., substeps)?;
            let submitted = start.elapsed();
            let waiting = Instant::now();
            let snapshot = gpu.wait_readback()?.unwrap();
            gpu.take_terrain_contacts();
            assert!(
                snapshot
                    .iter()
                    .all(|b| b.position.iter().all(|v| v.is_finite()))
            );
            wait.push(waiting.elapsed());
            submit.push(submitted);
            tick.push(start.elapsed());
        }
        println!(
            "{workload},{count},{substeps},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{}",
            percentile(&mut submit, 0.5),
            percentile(&mut submit, 0.95),
            percentile(&mut wait, 0.5),
            percentile(&mut tick, 0.5),
            percentile(&mut tick, 0.95),
            percentile(&mut tick, 0.99),
            count * std::mem::size_of::<Body>() + TERRAIN_EVENT_READBACK_BYTES
        );
    }
    Ok(())
}
