use bevy_app::App;
use simulation::{ServerConfig, Simulation, SimulationPlugin};
use std::{
    error::Error,
    time::{Duration, Instant},
};
use voxel_world::VoxelWorld;

fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(feature = "tracy")]
    {
        use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
        tracing_subscriber::registry()
            .with(tracing_tracy::TracyLayer::default())
            .init();
    }
    let mut config = ServerConfig::default();
    let mut ticks = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--help" {
            println!(
                "server [--bind 127.0.0.1:4000] [--seed 7] [--radius 1..64 (default 16)] [--ticks N] [--metrics-every 600] [--gpu-physics] [--no-titan] [--packages DIR]"
            );
            return Ok(());
        }
        if arg == "--gpu-physics" {
            config.gpu_physics = true;
            continue;
        }
        if arg == "--no-titan" {
            config.spawn_titan = false;
            continue;
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {arg}"))?;
        match arg.as_str() {
            "--bind" => config.bind = value.parse()?,
            "--seed" => config.seed = value.parse()?,
            "--packages" => config.packages = value.into(),
            "--radius" => {
                config.radius = value.parse()?;
                if !(1..=protocol::MAX_VIEW_RADIUS).contains(&config.radius) {
                    return Err(
                        format!("--radius must be 1..={}", protocol::MAX_VIEW_RADIUS).into(),
                    );
                }
            }
            "--ticks" => ticks = Some(value.parse::<u64>()?),
            "--metrics-every" => config.metrics_every = value.parse()?,
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    println!(
        "view radius={} chunks ({}m), columns={}",
        config.radius,
        config.radius * voxel_world::CHUNK_SIZE,
        (2 * config.radius + 1).pow(2)
    );
    let plugin = SimulationPlugin::bind(config)?;
    println!(
        "server listening {} TCP+UDP authoritative_hz=60",
        plugin.local_addr()
    );
    let mut app = App::new();
    app.add_plugins(plugin);
    let step = Duration::from_nanos(1_000_000_000 / 60);
    let mut deadline = Instant::now();
    while ticks.is_none_or(|limit| app.world().resource::<Simulation>().tick < limit) {
        let now = Instant::now();
        if now < deadline {
            std::thread::sleep(deadline - now);
        }
        app.update();
        #[cfg(feature = "tracy")]
        tracing::info!(tracy.frame_mark = true);
        deadline += step;
        // At most three immediate catch-up ticks; discard excess wall-clock debt.
        if Instant::now().saturating_duration_since(deadline) > step * 3 {
            deadline = Instant::now();
        }
    }
    app.world()
        .resource::<Simulation>()
        .print_metrics(app.world().resource::<VoxelWorld>().chunks.len());
    Ok(())
}
