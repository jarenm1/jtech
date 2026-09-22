// Triplanar terrain texturing over the standard PBR input.
//
// Vertex data contract (set by `mesh_chunk`):
//   color.rgb  blended material tint
//   color.a    blend weight toward the second material (0..0.4)
//   uv.x       primary material id
//   uv.y       secondary material id
// The atlas is grayscale luminance; tint comes from vertex color so material
// blending stays a pure color lerp.

#import bevy_pbr::{
    forward_io::{VertexOutput, FragmentOutput},
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{alpha_discard, apply_pbr_lighting, main_pass_post_lighting_processing},
    pbr_types::STANDARD_MATERIAL_FLAGS_UNLIT_BIT,
}

@group(3) @binding(100) var atlas: texture_2d<f32>;
@group(3) @binding(101) var atlas_sampler: sampler;

const ATLAS_TILES: f32 = 8.0;
// One texture tile spans four meters of world surface.
const TEX_SCALE: f32 = 0.25;

fn tile_uv(material: f32, uv: vec2<f32>) -> vec2<f32> {
    let tile = clamp(u32(material + 0.5), 0u, u32(ATLAS_TILES) - 1u);
    return vec2<f32>((f32(tile) + fract(uv.x)) / ATLAS_TILES, fract(uv.y));
}

fn triplanar(material: f32, world_pos: vec3<f32>, weights: vec3<f32>) -> f32 {
    let p = world_pos * TEX_SCALE;
    let sx = textureSample(atlas, atlas_sampler, tile_uv(material, p.zy)).r;
    let sy = textureSample(atlas, atlas_sampler, tile_uv(material, p.xz)).r;
    let sz = textureSample(atlas, atlas_sampler, tile_uv(material, p.xy)).r;
    return sx * weights.x + sy * weights.y + sz * weights.z;
}

@fragment
fn fragment(
    vertex_output: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
    var in = vertex_output;
    var pbr_input = pbr_input_from_standard_material(in, is_front);

    // Triplanar weights: sharp falloff keeps each axis dominant on its faces.
    var weights = abs(in.world_normal);
    weights = weights * weights * weights * weights;
    weights = weights / (weights.x + weights.y + weights.z);

    let primary = triplanar(in.uv.x, in.world_position.xyz, weights);
    var luminance = primary;
#ifdef VERTEX_COLORS
    if in.color.a > 0.001 {
        let secondary = triplanar(in.uv.y, in.world_position.xyz, weights);
        luminance = mix(primary, secondary, in.color.a);
    }
#endif

    pbr_input.material.base_color = vec4<f32>(
        pbr_input.material.base_color.rgb * luminance,
        pbr_input.material.base_color.a,
    );
    pbr_input.material.base_color = alpha_discard(
        pbr_input.material,
        pbr_input.material.base_color,
    );

    var out: FragmentOutput;
    if (pbr_input.material.flags & STANDARD_MATERIAL_FLAGS_UNLIT_BIT) == 0u {
        out.color = apply_pbr_lighting(pbr_input);
    } else {
        out.color = pbr_input.material.base_color;
    }
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}
