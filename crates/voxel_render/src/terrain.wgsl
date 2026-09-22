// Triplanar terrain texturing over the standard PBR input.
//
// Vertex data contract (set by `mesh_chunk`):
//   color.rgb  white (texture carries the full material color)
//   color.a    blend weight toward the second material (0..0.4)
//   uv.x       primary material id
//   uv.y       secondary material id
// The atlas is a horizontal row of RGB tiles; the sampler wraps so tile UVs
// stay continuous across the tile edge (keeps derivatives clean for mips).

#import bevy_pbr::{
    forward_io::{VertexOutput, FragmentOutput},
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{alpha_discard, apply_pbr_lighting, main_pass_post_lighting_processing},
    pbr_types::STANDARD_MATERIAL_FLAGS_UNLIT_BIT,
}

@group(3) @binding(100) var atlas: texture_2d_array<f32>;
@group(3) @binding(101) var atlas_sampler: sampler;

const ATLAS_TILES: i32 = 8;
// One texture tile spans four meters of world surface.
const TEX_SCALE: f32 = 0.25;

fn layer(material: f32) -> i32 {
    return clamp(i32(material + 0.5), 0, ATLAS_TILES - 1);
}

fn triplanar(material: f32, world_pos: vec3<f32>, weights: vec3<f32>) -> vec3<f32> {
    let p = world_pos * TEX_SCALE;
    let tile = layer(material);
    let sx = textureSample(atlas, atlas_sampler, p.zy, tile).rgb;
    let sy = textureSample(atlas, atlas_sampler, p.xz, tile).rgb;
    let sz = textureSample(atlas, atlas_sampler, p.xy, tile).rgb;
    return sx * weights.x + sy * weights.y + sz * weights.z;
}

@fragment
fn fragment(
    vertex_output: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
    var in = vertex_output;
    var pbr_input = pbr_input_from_standard_material(in, is_front);

    // Triplanar weights: steep falloff keeps side projections off slopes —
    // their edge-on smear reads as streaks on gentle terrain.
    var weights = abs(in.world_normal);
    weights = weights * weights * weights * weights;
    weights = weights * weights;
    weights = weights / (weights.x + weights.y + weights.z);

    var texel = triplanar(in.uv.x, in.world_position.xyz, weights);
#ifdef VERTEX_COLORS
    if in.color.a > 0.001 {
        let secondary = triplanar(in.uv.y, in.world_position.xyz, weights);
        texel = mix(texel, secondary, in.color.a);
    }
#endif

    pbr_input.material.base_color = vec4<f32>(
        pbr_input.material.base_color.rgb * texel,
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
