// Cached, terrain-conforming blade ribbons; only wind and distance LOD run per frame.
// position: world-space rest geometry, normal: upward-biased lighting normal.
// color: (height fraction, stable seed, full blade height, density rank).
// uv: world-space root XZ. Patch entities have identity transforms.
#import bevy_pbr::{
    forward_io::{Vertex, VertexOutput, FragmentOutput},
    mesh_functions,
    mesh_view_bindings::{globals, view},
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},
    view_transformations::position_world_to_clip,
}

@group(3) @binding(100) var<uniform> lod_limits: vec4<f32>;
@group(3) @binding(101) var atlas: texture_2d_array<f32>;
@group(3) @binding(102) var atlas_sampler: sampler;
@group(3) @binding(103) var<uniform> density: vec4<f32>;

@vertex
fn vertex(vertex: Vertex) -> VertexOutput {
    var out: VertexOutput;
    let world_from_local = mesh_functions::get_world_from_local(vertex.instance_index);
    let t = vertex.color.x;
    let seed = vertex.color.y;
    let root = vec3<f32>(vertex.uv.x, vertex.position.y - t * vertex.color.z, vertex.uv.y);
    let distance = length(root.xz - view.world_position.xz);
    // Nested stable subsets survive into the distance. CPU patches omit a
    // subset only after every blade in it has collapsed in this shader.
    var max_end = lod_limits.x;
    if vertex.color.w < density.y { max_end = lod_limits.y; }
    if vertex.color.w < density.z { max_end = lod_limits.z; }
    let end = mix(max_end * 0.75, max_end, seed);
    let lod = 1.0 - smoothstep(end - max_end * 0.25, end, distance);
    var offset = vertex.position - root;
    // Slightly wider distant silhouettes compensate for lower density.
    let width = mix(1.0, 1.8, smoothstep(lod_limits.x * 0.5, lod_limits.y, distance));
    offset.x *= width;
    offset.z *= width;
    var local = root + offset * lod;
    let wind_dir = vec2<f32>(0.85, 0.53);
    let wave = sin(dot(root.xz, wind_dir) * 0.55 - globals.time * 1.8);
    let flutter = sin(globals.time * 3.1 + seed * 6.283185 + dot(root.xz, vec2<f32>(1.7, 0.9)));
    let bend = (0.18 * wave + 0.045 * flutter) * t * t * vertex.color.z * lod;
    local.x += bend * wind_dir.x;
    local.z += bend * wind_dir.y;
    out.world_position = mesh_functions::mesh_position_local_to_world(world_from_local, vec4<f32>(local, 1.0));
    out.position = position_world_to_clip(out.world_position.xyz);
    out.world_normal = mesh_functions::mesh_normal_local_to_world(vertex.normal, vertex.instance_index);
    out.color = vertex.color;
    out.uv = vertex.uv;
#ifdef VERTEX_OUTPUT_INSTANCE_INDEX
    out.instance_index = vertex.instance_index;
#endif
    return out;
}

@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    var pbr = pbr_input_from_standard_material(in, is_front);
    // Use the same sRGB atlas/layer/world scale as terrain, including its
    // local green variation. A mip-filtered root sample avoids stripy blades.
    let ground = textureSampleLevel(atlas, atlas_sampler, in.uv * 0.25, i32(lod_limits.w), 2.0).rgb;
    let shade = mix(0.85, 1.05, smoothstep(0.0, 1.0, in.color.x));
    pbr.material.base_color = vec4<f32>(ground * shade, 1.0);
    // Both faces share an upward lighting normal instead of turning black when
    // the camera crosses a ribbon. These are foliage normals, not flat faces.
    pbr.N = normalize(vec3<f32>(in.world_normal.x, abs(in.world_normal.y), in.world_normal.z));
    var out: FragmentOutput;
    out.color = apply_pbr_lighting(pbr);
    out.color = main_pass_post_lighting_processing(pbr, out.color);
    return out;
}
