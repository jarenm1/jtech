//! Procedural texture atlas: one 128x128 RGB tile per material, laid out in a
//! horizontal row, with mipmaps. The triplanar shader samples the tile by
//! material id; vertex color stays white so the texture carries the full
//! material color. Material blending happens in texture space via `uv0` +
//! vertex alpha.
//!
//! All noise is periodic on the tile lattice so textures tile seamlessly.

use bevy::{
    asset::RenderAssetUsages,
    image::{Image, ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor},
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
};

pub const ATLAS_TILE: usize = 128;
pub const ATLAS_TILES: usize = 8;
const MIP_LEVELS: u32 = 8; // 128 -> 1

/// Shadow / mid / highlight tones per material (sRGB hex). The noise field
/// picks a point on this ramp per texel, so hue varies inside a material.
const PALETTE: [[u32; 3]; ATLAS_TILES] = [
    [0x000000, 0x000000, 0x000000], // air: unused
    [0x162E24, 0x2D4A3E, 0x5A7D6A], // grass: pine / ivy / sage
    [0x1C1613, 0x3D3028, 0x5C4D43], // dirt: obsidian / umber / clay
    [0x1D2226, 0x3C444B, 0x616D75], // stone: slate / flint / lichen
    [0x2A2118, 0x4A3C2C, 0x6B5B45], // sand: dark ochre ramp
    [0x1A120C, 0x2A1D14, 0x453322], // wood: dark timber ramp
    [0x241318, 0x3A2430, 0x54384A], // bedroll: dark wine ramp
    [0x000000, 0x000000, 0x000000], // unused
];

/// Builds the material atlas: a 128x128 texture array, layer = material id,
/// each layer with a full mip chain (LayerMajor order).
pub fn texture_atlas() -> Image {
    let mut data = Vec::new();
    for material in 0..ATLAS_TILES {
        let mut level = base_level(material as u8);
        let mut width = ATLAS_TILE;
        for _ in 0..MIP_LEVELS {
            data.extend_from_slice(&level);
            level = downsample(&level, width);
            width /= 2;
        }
    }
    // `Image::new` asserts data == mip-0 size; build directly for mip data.
    let mut image = Image::new_uninit(
        Extent3d {
            width: ATLAS_TILE as u32,
            height: ATLAS_TILE as u32,
            depth_or_array_layers: ATLAS_TILES as u32,
        },
        TextureDimension::D2,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.data = Some(data);
    image.texture_descriptor.mip_level_count = MIP_LEVELS;
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        anisotropy_clamp: 16,
        ..Default::default()
    });
    image
}

/// Mip 0 for one material layer at full resolution.
fn base_level(material: u8) -> Vec<u8> {
    let mut data = vec![0u8; ATLAS_TILE * ATLAS_TILE * 4];
    for y in 0..ATLAS_TILE {
        for x in 0..ATLAS_TILE {
            let rgb = tile_texel(material, x, y);
            let i = (y * ATLAS_TILE + x) * 4;
            data[i..i + 3].copy_from_slice(&rgb);
            data[i + 3] = 255;
        }
    }
    data
}


/// Box-filters one mip level down by 2x.
fn downsample(level: &[u8], w: usize) -> Vec<u8> {
    let h = w; // square tiles
    let (w2, h2) = (w / 2, h / 2);
    let mut out = vec![0u8; w2 * h2 * 4];
    for y in 0..h2 {
        for x in 0..w2 {
            let mut sum = [0u32; 4];
            for dy in 0..2 {
                for dx in 0..2 {
                    let i = ((y * 2 + dy) * w + x * 2 + dx) * 4;
                    for c in 0..4 {
                        sum[c] += level[i + c] as u32;
                    }
                }
            }
            let o = (y * w2 + x) * 4;
            for c in 0..4 {
                out[o + c] = (sum[c] / 4) as u8;
            }
        }
    }
    out
}

fn tile_texel(material: u8, x: usize, y: usize) -> [u8; 3] {
    let [shadow, mid, high] = PALETTE[material as usize];
    // Position on the shadow->highlight ramp: broad mottle plus fine detail.
    let ramp = match material {
        // Stone: strongest high-frequency grain.
        3 => 0.15 + 0.7 * fbm(x, y, 32, 3, 31),
        // Wood: grain stretched vertically (low x frequency).
        5 => 0.15 + 0.7 * fbm_aniso(x, y, 4, 16, 2, 51),
        // Bedroll: soft cloth, low contrast.
        6 => 0.3 + 0.4 * fbm(x, y, 4, 2, 61),
        // Grass/dirt/sand: mid-frequency mottle + fine speckle.
        _ => 0.1 + 0.6 * fbm(x, y, 16, 2, 11 + material as u64 * 10)
            + 0.2 * fbm(x, y, 64, 1, 12 + material as u64 * 10),
    };
    let ramp = ramp.clamp(0.0, 1.0);
    let (a, b, t) = if ramp < 0.5 {
        (shadow, mid, ramp * 2.0)
    } else {
        (mid, high, (ramp - 0.5) * 2.0)
    };
    let (ar, ag, ab) = unpack(a);
    let (br, bg, bb) = unpack(b);
    [
        (ar + (br - ar) * t) as u8,
        (ag + (bg - ag) * t) as u8,
        (ab + (bb - ab) * t) as u8,
    ]
}

fn unpack(hex: u32) -> (f32, f32, f32) {
    (
        (hex >> 16 & 0xFF) as f32,
        (hex >> 8 & 0xFF) as f32,
        (hex & 0xFF) as f32,
    )
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

    #[test]
    fn atlas_has_full_mip_chain() {
        let image = texture_atlas();
        assert_eq!(image.texture_descriptor.mip_level_count, MIP_LEVELS);
        // Per layer: 128 + 64 + ... + 1 pixels, 4 bytes each.
        let pixels_per_layer: usize = (0..MIP_LEVELS)
            .map(|m| (ATLAS_TILE >> m) * (ATLAS_TILE >> m))
            .sum();
        assert_eq!(
            image.data.as_ref().unwrap().len(),
            pixels_per_layer * ATLAS_TILES * 4
        );
    }
}
