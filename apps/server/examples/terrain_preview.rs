//! Render a package's elevation and biome maps without starting a server or GPU.
//! cargo run -p server --example terrain_preview -- /tmp/terrain.png 7 ./packages
use std::{collections::BTreeMap, fs::File, io::BufWriter, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let output = PathBuf::from(args.next().unwrap_or_else(|| "terrain.png".into()));
    let seed: u64 = args.next().unwrap_or_else(|| "7".into()).parse()?;
    let directory = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(game_packages::default_directory);
    let generator = game_packages::load_terrain(&directory)?;
    const SIZE: usize = 512;
    let step: i64 = args.next().unwrap_or_else(|| "16".into()).parse()?;
    if !(1..=1024).contains(&step) {
        return Err("blocks per pixel must be in 1..=1024".into());
    }
    let mut samples = Vec::with_capacity(SIZE * SIZE);
    let mut biomes = BTreeMap::new();
    let mut low = i32::MAX;
    let mut high = i32::MIN;
    for z in 0..SIZE {
        for x in 0..SIZE {
            let sample = generator.sample(
                (x as i64 - SIZE as i64 / 2) * step,
                (z as i64 - SIZE as i64 / 2) * step,
                seed,
            );
            low = low.min(sample.height);
            high = high.max(sample.height);
            *biomes.entry(sample.biome.0).or_insert(0usize) += 1;
            samples.push(sample);
        }
    }
    let mut pixels = vec![0u8; SIZE * SIZE * 2 * 3];
    let palette = [
        [99, 159, 70],
        [226, 193, 123],
        [170, 193, 187],
        [39, 107, 66],
        [167, 115, 156],
        [197, 140, 78],
        [82, 146, 167],
        [187, 184, 67],
    ];
    for z in 0..SIZE {
        for x in 0..SIZE {
            let sample = samples[z * SIZE + x];
            let west = samples[z * SIZE + x.saturating_sub(1)].height;
            let north = samples[z.saturating_sub(1) * SIZE + x].height;
            let slope = ((sample.height - west) + (sample.height - north)) as f32 / step as f32;
            let shade = (0.85 - slope * 0.24).clamp(0.28, 1.15);
            let altitude =
                ((sample.height - low) as f32 / (high - low).max(1) as f32).clamp(0.0, 1.0);
            let color = [
                70.0 + altitude * 155.0,
                115.0 + altitude * 100.0,
                48.0 + altitude * 162.0,
            ];
            let left = (z * SIZE * 2 + x) * 3;
            for channel in 0..3 {
                pixels[left + channel] = (color[channel] * shade).min(255.0) as u8;
            }
            let right = (z * SIZE * 2 + SIZE + x) * 3;
            pixels[right..right + 3]
                .copy_from_slice(&palette[sample.biome.0 as usize % palette.len()]);
        }
    }
    // Mark world origin in both panels.
    for panel in 0..2 {
        for offset in -3..=3 {
            for (x, z) in [
                (SIZE as i32 / 2 + offset, SIZE as i32 / 2),
                (SIZE as i32 / 2, SIZE as i32 / 2 + offset),
            ] {
                let i = (z as usize * SIZE * 2 + panel * SIZE + x as usize) * 3;
                pixels[i..i + 3].copy_from_slice(&[255, 50, 50]);
            }
        }
    }
    let mut encoder = png::Encoder::new(
        BufWriter::new(File::create(&output)?),
        (SIZE * 2) as u32,
        SIZE as u32,
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&pixels)?;
    println!(
        "{}: left=elevation/hillshade, right=biomes, red=origin; {} blocks across",
        output.display(),
        SIZE as i64 * step
    );
    println!(
        "identity={} version={} seed={seed} heights={low}..{high} biome_samples={biomes:?}",
        generator.identity(),
        generator.version()
    );
    Ok(())
}
