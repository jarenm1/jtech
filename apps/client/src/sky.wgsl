// Procedural sky dome.
//
// Rendered on a camera-centred sphere that sits behind all terrain. The
// fragment shader turns the view ray into a horizon-to-zenith gradient, adds a
// sun glow and disk, a moon disk, and a star field, and is driven entirely by
// the day-cycle uniforms in `SkyMaterial`.
//
// Uniform layout (set by `SkyMaterial::from_lighting`):
//   sun.xyz        direction toward the sun (unit)
//   sun.w          sun disk visibility, 0 below the horizon .. 1 overhead
//   sun_color.rgb  linear sun tint
//   sun_color.a    sun glow strength (persists slightly past sunset)
//   horizon.rgb    linear horizon color
//   horizon.a      star intensity, 0 by day .. 1 at night
//   zenith.rgb     linear zenith color
//   zenith.a       moon visibility, 0 by day .. 1 at night

#import bevy_pbr::forward_io::{VertexOutput, FragmentOutput}
#import bevy_pbr::mesh_view_bindings::view

@group(3) @binding(0) var<uniform> sun: vec4<f32>;
@group(3) @binding(1) var<uniform> sun_color: vec4<f32>;
@group(3) @binding(2) var<uniform> horizon: vec4<f32>;
@group(3) @binding(3) var<uniform> zenith: vec4<f32>;

// Cool moonlight tint for the moon disk and glow.
const MOON_TINT: vec3<f32> = vec3<f32>(0.62, 0.72, 0.95);

// Hash from Dave Hoskins, "Hash without Sine".
fn hash3(p: vec3<f32>) -> f32 {
    var q = fract(p * 0.1031);
    q = q + vec3<f32>(dot(q, q.yzx + 33.33));
    return fract((q.x + q.y) * q.z);
}

// Sparse points on a jittered lattice, each a soft dot near its cell centre.
// The lattice is coarse enough that a star spans a couple of pixels at 1080p.
fn star_field(dir: vec3<f32>) -> f32 {
    let p = dir * 140.0;
    let cell = floor(p);
    let local = fract(p) - 0.5;
    let jitter = vec3<f32>(hash3(cell + 11.0), hash3(cell + 23.0), hash3(cell + 37.0)) - 0.5;
    let d = length(local - jitter * 0.7);
    return smoothstep(0.997, 1.0, hash3(cell)) * smoothstep(0.45, 0.0, d);
}

@fragment
fn fragment(in: VertexOutput) -> FragmentOutput {
    let dir = normalize(in.world_position.xyz - view.world_position);

    // Gradient: horizon color at the horizon, zenith color overhead.
    let up = clamp(dir.y, 0.0, 1.0);
    var color = mix(horizon.rgb, zenith.rgb, pow(up, 0.45));

    // Sun: a broad glow plus a soft disk, both fading out below the horizon.
    let sun_dot = max(dot(dir, sun.xyz), 0.0);
    color += sun_color.rgb * pow(sun_dot, 16.0) * sun_color.a;
    color = mix(color, sun_color.rgb * 1.5, smoothstep(0.9997, 0.99992, sun_dot) * sun.w);

    // Moon: opposite the sun, visible at night.
    let moon_dot = max(dot(dir, -sun.xyz), 0.0);
    color += MOON_TINT * pow(moon_dot, 20.0) * zenith.a;
    color = mix(color, MOON_TINT * 1.3, smoothstep(0.99985, 0.99997, moon_dot) * zenith.a);

    // Stars: sparse points above the horizon, faded by daylight.
    if horizon.a > 0.001 {
        color += vec3<f32>(star_field(dir) * horizon.a * smoothstep(0.0, 0.3, dir.y));
    }

    return FragmentOutput(vec4<f32>(color, 1.0));
}
