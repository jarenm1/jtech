# Terrain

Terrain is generated from a compiled native graph. Scheme authors the graph in
`packages/terrain/server.scm`; the server evaluates that file once and compiles
it into `voxel_world::terrain::TerrainGenerator`. Chunk generation then walks the
compiled graph, so no Scheme runs per voxel or per chunk.

## World layout

| Constant | Value | Meaning |
| --- | --- | --- |
| `CHUNK_SIZE` | 32 | blocks per chunk axis |
| `MIN_CHUNK_Y` | -8 | lowest generated chunk layer |
| `MAX_CHUNK_Y` | 23 | highest generated chunk layer |
| `WORLD_MIN_Y` | -256 | lowest world block |
| `WORLD_MAX_Y` | 767 | highest world block |

Surface heights are clamped to -255 through 763, leaving a floor and spawn
headroom inside the world. Chunks above the surface use uniform air; chunks below
the deepest possible soil use uniform stone without visiting individual cells.

## Native API

```rust
pub struct TerrainGenerator;

impl TerrainGenerator {
    pub fn compile(spec: TerrainSpec) -> Result<Self, String>;
    pub fn default() -> Self;
    pub fn identity(&self) -> &str;
    pub fn version(&self) -> u32;
    pub fn node_count(&self) -> usize;
    pub fn biome_count(&self) -> usize;
    pub fn biome_name(&self, biome: Biome) -> &str;
    pub fn sample(&self, x: i64, z: i64, seed: u64) -> TerrainSample;
    pub fn column_bounds(&self, cx: i32, cz: i32, seed: u64) -> (i32, i32);
}

pub struct TerrainSample {
    pub height: i32,
    pub biome: Biome,
    pub surface: u8,
    pub subsurface: u8,
    pub soil: u8,
    pub temperature: f32,
    pub moisture: f32,
}
```

`sample` is deterministic in `(x, z, seed)` and independent of call order; it is
the same column the generator would place in a chunk. `surface`, `subsurface`
and `soil` are the biome rules before the slope override. `column_bounds`
returns the exact minimum and maximum surface heights of the 32x32 columns of
chunk `(cx, cz)`.

```rust
pub struct Chunk;

impl Chunk {
    pub fn generate(coord: IVec3, seed: u64) -> Self;          // built-in default graph
    pub fn generate_with(coord: IVec3, seed: u64, generator: &TerrainGenerator) -> Self;
}
```

`VoxelWorld` carries `pub generator: Arc<TerrainGenerator>`; `ensure_chunk` uses
it. `VoxelWorld::default()` installs `TerrainGenerator::default()`.

## Loading a Scheme package

```rust
use game_packages::load_terrain;

let generator = load_terrain(std::path::Path::new("packages"))?;
```

`load_terrain(directory)` evaluates `directory/terrain/server.scm` once inside
the bounded Scheme sandbox and watchdog, validates the declared graph and biome
table, and compiles them. The API version is independent of the weapon packages.

Start with `cargo run -p server -- --packages ./packages`. A missing or invalid
terrain package prevents server startup and prints a diagnostic. Only install
trusted packages: the VM watchdog is cooperative, not a hard process or memory
sandbox.

The server pins the compiled generator and seed for its lifetime. Edit the
package, then restart the server to create a world with the new terrain. Terrain
is not hot-reloaded. Chunk eviction regenerates from the pinned graph and reapplies
the in-memory edit journal. Worlds and edit journals currently have no disk-save
format. Increment `terrain-version` when changing the generator's behavior.

## Authoring

The package declares `terrain-api-version`, `terrain-version`,
`generator-identity`, `stone-slope`, four expression values and a biome list:

```
(define terrain-api-version 1)
(define terrain-version 1)
(define generator-identity "jtech-terrain-v1")
(define stone-slope 4.0)

(define terrain-height
  (tadd (constant 40.0) (tmul (fbm 0.01 3 2.0 0.5 1) (constant 12.0))))
(define terrain-temperature (constant 0.5))
(define terrain-moisture (constant 0.5))
(define terrain-soil (constant 0.5))

(define biomes
  (list (biome "plains" 'grass 'dirt 4 -2.0 3.0 -2.0 3.0 -1000.0 5000.0)))
```

Primitives:

| Primitive | Result |
| --- | --- |
| `(terrain-x)` `(terrain-z)` | world column coordinates |
| `(constant v)` | literal |
| `(fbm freq octaves lacunarity gain salt)` | fBm value noise in `[0, 1]` |
| `(fbm-xy freq octaves lacunarity gain salt x z)` | fBm on explicit coordinates |
| `(ridged ...)` `(ridged-xy ...)` | folded ridged noise in `[0, 1]` |
| `(tadd a b)` `(tsub a b)` `(tmul a b)` `(tdiv a b)` | arithmetic (`tdiv` by ~0 is 0) |
| `(tmin a b)` `(tmax a b)` | extrema |
| `(tabs v)` `(tneg v)` `(tsqrt v)` `(tpow v e)` | unary |
| `(tclamp v lo hi)` | clamp |
| `(tmix a b t)` | linear blend |
| `(tsmoothstep e0 e1 x)` | smooth 0..1 ramp |
| `(tstep edge x)` | hard step |
| `(tsmooth-min a b k)` `(tsmooth-max a b k)` | polynomial smooth extrema |
| `(tscale-bias v scale bias)` | `v * scale + bias` |
| `(tcurve x (list (list x0 y0) ...))` | smooth piecewise curve |
| `(species name model spacing density ...)` | one scatter species; see [organic scatter](#organic-scatter) |
| `(scatter-rule biome (list species ...))` | biome membership for scatter |

Each noise field carries its own `salt`; the seed is mixed with the salt so
different seeds decorrelate every field. Graph size is capped at 512 nodes and
64 levels of nesting, and constants, noise parameters and curve points must be
finite. A value that evaluates to a non-finite number is rejected at compile
time and replaced with 0 if it ever appears at runtime.

The biome constructor takes:

```scheme
(biome name surface subsurface depth temp-min temp-max moisture-min moisture-max height-min height-max)
```

Biome rules are evaluated in declaration order; earlier rules claim coverage
first and the last biome is the fallback. Membership fades inward across 0.08
temperature/moisture units and 12 height blocks at range edges. Soil depths are
blended, and a seeded world-coordinate hash chooses the surface/subsurface
material pair from those weights. Transitions
therefore have mixed materials instead of straight palette boundaries.
`surface` and `subsurface` accept `'grass`, `'dirt`, `'stone` and `'sand`.
The soil factor scales the blended depth. During generation, the slope override
replaces surface and soil with stone when the largest neighboring height delta
reaches `stone-slope`.

## Default world

The shipped package and `TerrainGenerator::default()` describe the same graph:
broad plains around y=44 with gentle relief, a concentrated ridged mountain field
reaching roughly y=350, and a connected valley network carving up to 38 blocks
through the plains. Temperature is cooled by the column's actual height, so high
ground is cold, and the biome table is shore, desert, mountains, tundra and
plains. The `game_packages` tests sample both authoring sources on the same grid
and require identical columns, so the native default cannot drift from the
shipped package.

Measured on a 256x256 grid of columns 128 blocks apart (seed 2024): median 42,
p05 7, p10 9, p90 94, p99 201, maximum 339. Sixty-three percent of columns fall
in 25..=90, 3.3% reach 150 or higher, 0.23% reach 250, 31% sit below 30 and 10%
below 10. Coverage is roughly 91% plains, 5% tundra, 3% mountains, 1% desert and
a small shore fringe; only 0.07% of columns touch the clamp floor.

## Organic scatter

Terrain also places organic objects — trees, boulders, vegetation — as mesh
instances tied to biomes. The package declares a species table and per-biome
membership:

```scheme
(define scatter-species
  (list
    (species "oak" "oak.glb" 6.0 0.5 0.6 0.0 120.0 0.25 1.0 0.8 1.4 0.15 0.012 0.52)
    (species "pine" "pine.glb" 5.0 0.45 0.8 0.0 200.0 0.15 1.0 0.9 1.6 0.15 0.014 0.5)
    (species "boulder" "boulder.glb" 9.0 0.3 1.5 0.0 400.0 0.0 1.0 0.6 1.4 0.35 0.0 0.0)))

(define scatter-rules
  (list
    (scatter-rule "plains" (list "oak" "boulder"))
    (scatter-rule "tundra" (list "pine" "boulder"))))
```

`(species name model spacing density slope-max min-altitude max-altitude
min-moisture max-moisture min-scale max-scale sink cluster-frequency
cluster-threshold)`:

| Field | Meaning |
| --- | --- |
| `model` | `.glb` under the package's `assets/`; validated at load |
| `spacing` | jittered-grid cell size in blocks — the minimum spacing |
| `density` | probability in `0..=1` that a candidate cell yields an instance |
| `slope-max` | largest neighbouring height delta, in blocks per block |
| `altitude` | absolute world-height band in blocks |
| `moisture` | moisture band in `0..=1` |
| `scale` | uniform scale range |
| `sink` | blocks to bury the model base, in `0..=8` |
| `cluster` | `(frequency, threshold)` low-frequency mask; frequency 0 disables |

A model's base is a flat plane, so on a slope its downhill edge floats by
`slope * footprint`. `sink` buries the base to hide that gap; boulders need
more than tree trunks because their footprint is wider. Zero places the origin
exactly on the surface.

Candidates come from a world-aligned jittered grid per species, so placement is
deterministic in `(x, z, seed)` and independent of chunk boundaries and call
order. A candidate survives when its density roll, cluster mask, biome
membership, altitude, moisture and slope all admit it. The cluster mask is what
turns uniform scatter into patches; `spacing` and `density` together set the
expected count per block.

Each instance is assigned to the chunk holding its surface block, so the
vertical chunk stack emits it exactly once. `scatter-rules` maps biome names to
species names; a biome without a rule stays bare. Unknown or duplicate names,
missing models and paths escaping `assets/` fail the package at load.

### Native API

```rust
pub struct ScatterInstance {
    pub species: u16,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub yaw: f32,
    pub scale: f32,
}

impl TerrainGenerator {
    pub fn species_count(&self) -> usize;
    pub fn species_name(&self, index: u16) -> &str;
    pub fn species_model(&self, index: u16) -> &str;
    pub fn scatter_chunk(&self, coord: IVec3, seed: u64) -> Vec<ScatterInstance>;
}
```

`y` is the rendered surface height less the species sink — the density ramp the
smooth mesher contours crosses zero at `raw_height`, not at the rounded block
top — so a model authored with its base at the origin sits on the ground.
Scatter is computed on the terrain worker thread alongside the chunk, never on
the simulation tick.

### Replication

Scatter is server-authoritative and streamed with the chunk: the `Chunk`
message carries the instances anchored in that chunk, and `Forget` evicts them
with it. The `Welcome` message announces the species table (name, package,
model) so clients resolve each instance's model. Models ship as package assets
under `packages/terrain/assets/` and download like any other package file; see
[gameplay packages](packages.md#package-assets).

## Grass blades

Grass is client-side decoration in `voxel_render`, not replicated scatter or
collision geometry. Broad, curved, matte ribbons match the low-poly trees:
five vertices and three opaque triangles per blade, no alpha-card textures.
Roots are sampled directly on the smooth terrain's grass triangles, rather
than rounded voxel tops. Non-grass and near-vertical faces are excluded;
placed solid blocks suppress blades they would cover. Blade colour is a
mip-filtered sample of the shared terrain atlas at the root, so blades always
match the ground beneath them.

The renderer keeps world-grid 8×8 m patches around the player, out to 128 m.
World-position seeds keep blades fixed when patches are rebuilt. Distance uses
nested rank subsets: near patches emit all blades, mid patches (~40–72 m) keep
30%, and far patches keep 12%, while each blade's rank also fades it into its
root over the last quarter of its range — the field thins gradually instead
of ending at a visible edge. Each patch records the revisions (including
missing chunks) of the terrain it consulted, so local edits, streaming, and
unloading invalidate it without rebuilding grass for unrelated distant chunks.
Surface contour meshes are shared between patches and released when no
resident patch depends on them.

At most two patches are built/uploaded per update plus backlog relief after
teleports, nearest first. Each build considers up to eight surface-bearing
chunks, with a 48 m vertical scan below the local ceiling. Dependency-stamp
scans run every 0.12 s or on a 4 m move; unchanged patches incur no contouring
or mesh uploads. Patch entities are frustum-culled with wind-padded bounds;
grass does not cast shadows and does not participate in the depth prepass.

`grass.wgsl` handles coherent travelling wind, per-blade flutter, atlas-based
root shading, and rank-varied distance scaling into fixed roots. Collapsed
blades still cost vertex processing until their patch is culled; this is not
compute/indirect blade compaction. Sun/ambient lighting and fog use the
standard terrain PBR pipeline.

The ribbon/patch/gradual-LOD approach follows the concepts in
[AMD GPUOpen's procedural grass reference](https://gpuopen.com/learn/mesh_shaders/mesh_shaders-procedural_grass_rendering/).
This implementation uses Bevy's portable vertex pipeline, not AMD's hardware
mesh-shader implementation. Geometry is cached on the CPU; animation and LOD
are evaluated on the GPU. Density, dimensions, patch radius, and rebuild limits
are the `GRASS_*` constants in `crates/voxel_render/src/lib.rs`.
`CLIENT_METRICS grass=` reports resident blades, not visible blades or triangles.

Regression coverage: `cargo test -p voxel_render` checks root contact with
surface-net triangles, deterministic rebuilding, revision/load/unload
invalidation, and suppression beneath placed blocks. Shader visibility and wind
also require running the graphical client; CPU tests do not validate rendering.

## Preview a package

```sh
cargo run -p server --example terrain_preview -- target/terrain-preview.png 7 ./packages
```

The PNG shows an 8,192-block-wide elevation/hillshade map on the left and biome
IDs on the right. Red crosses mark the origin. The command prints the height
range and biome sample counts without starting a game or requiring a GPU.
Append a fourth argument for blocks per pixel (1..1024, default 16). Use `2`
for a closer, 1,024-block-wide preview.

## Tests

```
cargo test -p voxel_world
cargo test -p game_packages
```

They cover determinism, call-order independence, seed sensitivity, chunk seams,
vertical layering, slope-aware stone surfaces, `column_bounds` versus generated
surfaces, terrain distribution (plains, valleys, mountains, multiple biomes),
graph bounds and validation, Scheme compilation of the shipped and a custom
graph, watchdog interruption of a runaway package, and scatter determinism,
seam-freeness, biome membership, ground placement and validation.
