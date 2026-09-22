//! Procedural grayscale texture atlas: one 128x128 luminance tile per
//! material, laid out in a horizontal row. The triplanar shader samples `.r`
//! and multiplies it into the vertex tint, so tiles are pure luminance noise
//! around 1.0 — hue lives in `voxel_world::block_color`.
//!
//! All noise is periodic on the tile lattice so textures tile seamlessly.

use bevy::{
    asset::RenderAssetUsages,
    image::Image,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
};

pub const ATLAS_TILE: usize = 128;
pub const ATLAS_TILES: usize = 8;

/// Builds the material atlas: tile index = material id.
pub fn texture_atlas() -> Image {
    let mut data = vec![0u8; ATLAS_TILE * ATLAS_TILE * ATLAS_TILES];
    for material in 0..ATLAS_TILES {
        for y in 0..ATLAS_TILE {
            for x in 0..ATLAS_TILE {
                data[material * ATLAS_TILE * ATLAS_TILE + y * ATLAS_TILE + x] =
                    tile_texel(material as u8, x, y);
            }
        }
    }
    Image::new(
        Extent3d {
            width: (ATLAS_TILE * ATLAS_TILES) as u32,
            height: ATLAS_TILE as u32,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::R8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    )
}

fn tile_texel(material: u8, x: usize, y: usize) -> u8 {
    // (base luminance, amplitude, recipe)
    let v = match material {
        // Grass: fine speckle over a soft mid-frequency mottle.
        1 => 0.82 + 0.18 * fbm(x, y, 16, 2, 11) + 0.06 * fbm(x, y, 64, 1, 12) - 0.03,
        // Dirt: broad blotches with sparse dark specks.
        2 => {
            let base = 0.80 + 0.20 * fbm(x, y, 8, 2, 21);
            if hash(x, y, 22) > 0.985 {
                base - 0.25
            } else {
                base
            }
        }
        // Stone: strongest high-frequency grain; geometry already bumps.
        3 => 0.74 + 0.26 * fbm(x, y, 32, 3, 31),
        // Sand: uniform fine grain, low contrast.
        4 => 0.86 + 0.14 * fbm(x, y, 32, 1, 41),
        // Wood: grain stretched vertically (low x frequency).
        5 => 0.76 + 0.24 * fbm_aniso(x, y, 4, 16, 2, 51),
        // Bedroll: soft cloth, lowest contrast.
        6 => 0.86 + 0.14 * fbm(x, y, 4, 2, 61),
        // Air/unused: neutral.
        _ => 1.0,
    };
    (v.clamp(0.0, 1.0) * 255.0) as u8
}

/// Periodic value-noise fBm in [0, 1]. `period` is lattice cells per tile.
fn fbm(x: usize, y: usize, period: usize, octaves: u32, seed: u64) -> f32 {
    fbm_aniso(x, y, period, period, octaves, seed)
}

fn fbm_aniso(x: usize, y: usize, px: usize, py: usize, octaves: u32, seed: u64) -> f32 {
    let mut sum = 0.0;
    let mut amp = 1.0;
    let mut norm = 0.0;
    let (mut px, mut py) = (px, py);
    for octave in 0..octaves {
        sum += amp * value_noise(x, y, px, py, seed + octave as u64 * 101);
        norm += amp;
        amp *= 0.5;
        px = (px * 2).min(ATLAS_TILE);
        py = (py * 2).min(ATLAS_TILE);
    }
    sum / norm
}

/// One octave of value noise on a wrapped `px * py` lattice.
fn value_noise(x: usize, y: usize, px: usize, py: usize, seed: u64) -> f32 {
    let fx = x as f32 * px as f32 / ATLAS_TILE as f32;
    let fy = y as f32 * py as f32 / ATLAS_TILE as f32;
    let x0 = fx.floor() as usize % px;
    let y0 = fy.floor() as usize % py;
    let x1 = (x0 + 1) % px;
    let y1 = (y0 + 1) % py;
    let tx = smooth(fx.fract());
    let ty = smooth(fy.fract());
    let a = hash(x0, y0, seed);
    let b = hash(x1, y0, seed);
    let c = hash(x0, y1, seed);
    let d = hash(x1, y1, seed);
    a + (b - a) * tx + (c - a) * ty + (a - b - c + d) * tx * ty
}

fn smooth(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Deterministic lattice hash in [0, 1).
fn hash(x: usize, y: usize, seed: u64) -> f32 {
    let mut h = seed
        .wrapping_mul(0x9E3779B97F4A7C15)
        .wrapping_add(x as u64)
        .wrapping_mul(0xBF58476D1CE4E5B9)
        .wrapping_add(y as u64);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58476D1CE4E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D049BB133111EB);
    h ^= h >> 31;
    (h >> 40) as f32 / (1u64 << 24) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_is_periodic_on_the_tile() {
        // The sampler wraps at tile edges; the noise lattice must wrap too or
        // seams appear. Sample one tile-width past the edge and require an
        // exact match — the lattice is periodic by construction.
        for period in [2, 4, 8, 16, 32, 64] {
            for y in 0..ATLAS_TILE {
                for x in 0..ATLAS_TILE {
                    assert_eq!(
                        fbm(x, y, period, 2, 7),
                        fbm(x + ATLAS_TILE, y, period, 2, 7),
                        "x seam at period {period}"
                    );
                    assert_eq!(
                        fbm(x, y, period, 2, 7),
                        fbm(x, y + ATLAS_TILE, period, 2, 7),
                        "y seam at period {period}"
                    );
                }
            }
        }
    }
}
