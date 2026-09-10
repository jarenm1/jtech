# Headless voxel physics

The server owns grid edits. Submit bounded GPU work, consume completed snapshots, then validate settling candidates against authoritative terrain and players before reinsertion.

```rust,no_run
use gpu_physics::{Body, GpuPhysics, Terrain};
let terrain = Terrain { origin: [0; 3], size: [16; 3], cells: vec![0; 4096] };
let mut gpu = GpuPhysics::new(terrain)?;
gpu.set_bodies(&[Body::new([8.5, 5.5, 8.5], 3)])?;
gpu.impulse(0, [3., 6., 0.])?;
// Each server tick, first consume any completed work.
if let Some(bodies) = gpu.try_readback()? {
    let contacts = gpu.take_terrain_contacts();
    // Apply terrain work after revalidating each target; remove destroyed() bodies.
    // Replicate positions. Validate bodies[i].settling_candidate() on CPU.
}
// false indicates backpressure or an empty body list; do not accumulate submissions.
gpu.submit(1. / 120., 2)?;
# Ok::<(), String>(())
```

Positions are unit-cube centers in world coordinates. Cell (x,y,z) occupies [x,x+1] × [y,y+1] × [z,z+1]. Material zero disables a body slot; IDs 1 through 5 match voxel_world materials. With `Terrain` and `GpuPhysics::new` / `new_async`, upload dense cells using `x + size_x * (z + size_z * y)`, at most 128 per axis. Outside dense terrain is solid.

Bodies occupy 48 bytes and persist in two GPU buffers. A batch uploads 48 parameter bytes and, only when impulses are queued, a 16-byte impulse record per active slot. Player input uses up to sixteen 32-byte `PlayerCollider` records; call idle-only `set_players(&players)` once per batch, or `set_players(&[])` to clear it. Positions are feet; collider half-width is 0.3 and height is 1.8. Player positions are fixed during a batch. The CPU owns player movement, swept contacts, and reactions from completed body snapshots.

Each substep applies a player motor, integrates, then performs four under-relaxed Jacobi contact iterations. Clear and rebuild the spatial grid before every iteration; ping-pong body buffers prevent read/write races. A final support pass advances settling once per substep. One submission and readback cover the entire batch. `try_readback` polls without waiting; `wait_readback` blocks for tests/benchmarks only. Spawn/remove through idle-only `set_bodies`, using the latest snapshot to preserve motion and damage. Terrain changes also require idle state. `update_terrain_range(start, cells)` uploads only the changed contiguous cells. Impulses queued during a submission apply to the next batch.

## Sparse chunk residency

Use `GpuPhysics::new_sparse()` for simultaneous contact regions anywhere in world space. Upload the complete initial resident set with `set_terrain_chunks(&chunks)`. Each entry is `([i32; 3], Vec<u32>)`: a chunk coordinate and exactly 32³ cells in x-fastest, then z, then y order. Chunk coordinates use floor division, so voxel -1 belongs to chunk -1, local coordinate 31. Materials must be 0 through 5.

For subsequent batches, call `update_sparse_terrain(&retained, &changed)`. `retained` is the **complete desired coordinate list**, including changed chunks; `changed` contains replacements only. Supply cell data for every new coordinate. Omit evicted coordinates from `retained`. Unchanged resident pages reuse their slots and require no cell upload. Both update methods return actual bytes uploaded and reject busy, duplicate, malformed, or over-capacity requests before changing CPU or GPU terrain.

```rust,no_run
use gpu_physics::{Body, GpuPhysics};
let mut gpu = GpuPhysics::new_sparse()?;
let west = [-40, 0, 0];
let east = [40, 0, 0];
gpu.set_terrain_chunks(&[(west, vec![0; 32768]), (east, vec![0; 32768])])?;
gpu.set_bodies(&[Body::new([-1271.5, 8.5, 8.5], 3), Body::new([1288.5, 8.5, 8.5], 3)])?;
// While idle, replace one chunk and retain the other without cloning its cells.
let uploaded = gpu.update_sparse_terrain(&[west, east], &[(west, vec![0; 32768])])?;
assert_eq!(uploaded, 16384);
# Ok::<(), String>(())
```

At most `MAX_TERRAIN_CHUNKS` (4,096) chunks are resident. Eight four-bit materials fit in one u32: each page costs 16 KiB. The page payload and an 8,192-entry open-addressed coordinate table occupy one 64.125 MiB storage binding. Membership changes upload the 128 KiB table; replacements upload only affected 16 KiB pages. CPU residency holds coordinates and slots, not a second copy of all cells.

Nonresident terrain is solid and produces no editable terrain-contact events. Keep chunks resident around every body throughout its swept motion, including the unit-cube extent and bounded contact projection. Retain overlapping regions until bodies move clear; choose activation, eviction, and capacity policy in the caller. Floating-point position precision still limits very large world coordinates.

`terrain.wgsl` contains the shared dense/sparse lookup; every terrain collision path uses it. `broadphase.wgsl` contains the reusable world-cell hash and clear/build passes. Material response and the small integration/contact/support kernels are in `physics.wgsl`.

## Contact and fracture model

`material(id)` exposes the shared Rust material table; shader constants are generated from that table. Density (kg/m³), damage-onset pressure (Pa), fracture budget (J/m³), fracture efficiency, attachment strength (N/m²), restitution, and friction are independent properties. For unit cubes, mass equals density and fracture threshold equals the material's fracture budget. Stone uses density 3, onset 2,000 Pa, fracture budget 60 J, efficiency 0.5, and attachment strength 1,500 N/m². A 12 J, 400 N, 0.01 m² tool contact adds 6 J with `Material::damage_energy`.

Normal contact work is the energy dissipated after restitution. Half belongs to each contacting material. Damage is that share times fracture efficiency times `smoothstep(1, 2, pressure / damage_onset)`. Contact force is impulse divided by a fixed 1/120-second measurement window, even for smaller integration steps. Restitution is suppressed below 2 m/s to stabilize resting contacts; material damage is pressure-gated rather than speed-gated. Body-pair work uses the impulse actually applied by the relaxed solver. Tangential friction is damping, not fracture damage in this slice.

After each simultaneous pair update, cap the pairwise damage allocation by the actual aggregate kinetic energy removed. Unassigned loss is damping/heat. This conservative budget accounts for Jacobi impulse cross-terms without treating retained kinetic energy as fracture work. Player penetration correction is limited to incoming body travel; a tiny contact velocity cannot unlock an arbitrary overlap shove.

`Body::damage_joules()` and `set_damage_joules()` preserve fractional accumulated joules. `destroyed()` compares against the unit-cube fracture limit. The server removes destroyed bodies. `damage_sleep` retains its high-bit support counter; `damage()` exposes saturated whole joules for diagnostics. At speeds below 0.6 m/s, terrain-supported bodies and slow stacks rooted through supported neighbors accumulate settling progress. After 24 substeps they become placement candidates. Lost support clears progress; every body integrates gravity. Player heads support collisions but do not count as permanent grid support.

### Terrain events

After `try_readback()` returns bodies, drain `take_terrain_contacts()`. Each event contains target voxel, material, dissipated energy already assigned to terrain, force, and actual overlapping face area. A face spanning several voxels divides its total work and impulse in proportion to overlap; no duplicated energy per voxel. Events are omitted when both pressure and attachment load are below onset. The conservative GPU attachment filter assumes one unit support face; the server revalidates actual attachments.

There are at most `MAX_TERRAIN_CONTACTS` (8,192) retained events per submission and in the CPU drain queue. Atomic append counts attempted events, bounds-checks every write, and increments the cumulative public `terrain_contact_overflow` on readback for GPU or undrained-queue losses. Readback includes a fixed 262,160-byte event region in addition to active body bytes. Out-of-region collision boundaries emit no voxel events. The server must release failed attachments at rest: the incoming impulse was already reacted into the fixed terrain during this batch. Only subsequent contacts accelerate the released body.

### Walking motor

Each player has a **total 60 N** horizontal push budget each substep. A bounded serial motor pass allocates at most `60 × dt` impulse across contacted bodies in slot order, accounting for already queued impulse. It runs once per substep, not once per solver iteration. Acceleration scales inversely with mass. Walking overlap does not positionally project a body; incoming body motion can still collide with a kinematic player. Player inputs require CPU collision validation. Slot-order allocation can favor earlier body slots in crowded contacts.

## Bounds

This backend supports at most 1,024 active blocks. The GPU hashes `floor(position / 2)` world cells into 1,024 buckets, independently of terrain residency or origin. Each bucket stores an atomic head and each body one linked-list entry. Contact and support passes traverse the 27 neighboring cells and filter each link by its exact world cell before processing it. Hash collisions, including two queried cells sharing a bucket, cannot duplicate contacts. There is no candidate truncation, coordinate clamp, or per-cell capacity. Separate passes order construction before traversal; contacts read predicted state and write separate output slots. Broadphase storage costs 4 KiB for heads and 4 KiB for links. Crowded buckets still have quadratic worst-case work.

Four bounded iterations improve short stacks, but are not a converged dense-pile or structural solver. Production islands and concentrated destruction require further testing before any 1,000-player capacity claim. Each velocity component is capped at 30 m/s and substeps at 1/120 second. Contact projection is capped per axis and swept against terrain. Bodies cannot tunnel through the expanded player AABB at that displacement bound; moving/teleporting player inputs require CPU sweeps. In impossible player/body/terrain overlaps, terrain clearance takes priority. There is no rotation, per-face attachment graph, continuous pair collision solver, or deformation.

## Verification

```sh
cargo test -p gpu_physics
cargo test -p gpu_physics --lib -- --include-ignored --test-threads=1
cargo run --release -p gpu_physics --example benchmark
cargo run --release -p gpu_physics --example benchmark -- --sparse-terrain
```

The benchmark prints adapter identity and initialization time, submit time, blocking completion/readback time, and end-to-end batch percentiles. Select packed chunk residency with `--sparse-terrain`; the default uses dense `Terrain`. “Sparse” in workload names describes the body arrangement. Backend cases use two substeps at 32, 128, 512, and 1,024 bodies. Server-batch cases use 128 bodies and six substeps, with sparse/piled arrangements and a pile with sixteen kinematic inputs uploaded each measured batch (512 bytes). Inputs are held at fixed positions with prescribed velocity to exercise contact transfer. Measurements exclude networking and surrounding server work; timings are CPU wall-clock, not isolated GPU timestamps.

Seventeen backend tests pass, including twelve adapter tests on software Vulkan. Sparse tests cover simultaneous distant signed-coordinate regions, real terrain events, page replacement, negative seams, residency misses, eviction, atomic validation, in-flight rejection, queued impulse slots, and colliding broadphase buckets. CPU tests cover packing, slot reuse, and terrain hash-table collisions.

### Software-adapter measurement, 2026-09-10

Packed chunk residency and hashed broadphase, using `--sparse-terrain`. Release build on `llvmpipe (LLVM 21.1.8, 256 bits) (Cpu)`, initialization **184.38 ms**. Four resident pages cover the 64×32×64 floor fixture. Ten warmup and 120 measured batches per case; terrain uploads and initialization are outside batch timing. Bodies are retained throughout each workload, so this measures solver/readback cost rather than server destruction transactions or streaming throughput.

| Workload | Bodies | Substeps | Submit p50 ms | Batch p50 ms | Batch p95 ms | Batch p99 ms | Readback bytes |
|---|---:|---:|---:|---:|---:|---:|---:|
| Backend sparse | 32 | 2 | 0.0967 | 1.4237 | 1.7526 | 2.0049 | 263,696 |
| Backend sparse | 128 | 2 | 0.0983 | 1.7308 | 2.2179 | 2.2592 | 268,304 |
| Backend sparse | 512 | 2 | 0.1054 | 2.7597 | 3.3598 | 3.4226 | 286,736 |
| Backend sparse | 1,024 | 2 | 0.1181 | 5.2799 | 6.0528 | 6.1652 | 311,312 |
| Server-batch sparse | 128 | 6 | 0.2882 | 5.1648 | 5.9372 | 6.0451 | 268,304 |
| Server-batch pile | 128 | 6 | 0.3057 | 10.4698 | 11.2705 | 11.7759 | 268,304 |
| Server-batch pile, 16 players | 128 | 6 | 0.3068 | 11.9228 | 12.8709 | 13.0841 | 268,304 |

The full workspace excluding socket-dependent networking passed **81 tests**, including software-Vulkan tests. Simulation coverage includes simultaneous distant explosions, repeated blasts through body-slot compaction, and playerless terrain streaming, chunk crossing, settlement, and eviction.

### Historical software-adapter measurement, 2026-09-10 (before sparse chunks and hashed broadphase)

Fracture/events, capped motor, and conservative pair-work budgeting. Release build on `llvmpipe (LLVM 21.1.8, 256 bits) (Cpu)`, initialization 179.41 ms. Same terrain, warmup, and sample count as below. Bodies are retained throughout the backend workload; this measures solver/readback cost rather than server destruction transactions.

| Workload | Bodies | Substeps | Submit p50 ms | Batch p50 ms | Batch p95 ms | Batch p99 ms | Readback bytes |
|---|---:|---:|---:|---:|---:|---:|---:|
| Backend sparse | 32 | 2 | 0.1133 | 1.6672 | 2.5165 | 3.0924 | 263,696 |
| Backend sparse | 128 | 2 | 0.1192 | 1.9063 | 2.8919 | 3.1243 | 268,304 |
| Backend sparse | 512 | 2 | 0.1210 | 2.4687 | 3.3844 | 3.7772 | 286,736 |
| Backend sparse | 1,024 | 2 | 0.1333 | 4.0663 | 5.2495 | 5.7203 | 311,312 |
| Server-batch sparse | 128 | 6 | 0.3186 | 5.4617 | 6.3362 | 6.7300 | 268,304 |
| Server-batch pile | 128 | 6 | 0.3223 | 8.2919 | 9.3474 | 9.6717 | 268,304 |
| Server-batch pile, 16 players | 128 | 6 | 0.3260 | 9.4386 | 10.7606 | 11.0661 | 268,304 |

At this measurement, twelve backend tests passed, including all nine adapter tests on software Vulkan. The full workspace excluding socket-dependent networking passed 52 tests, including 14 simulation lifecycle/material checks. Hardware and desktop acceptance need separate measurements.

### Historical software-adapter measurement, 2026-09-09 (before fracture/events and capped motor)

Release build, `llvmpipe (LLVM 21.1.8, 256 bits) (Cpu)`, initialization 293.14 ms. Ten warmup and 120 measured batches per case; 64×32×64 terrain, four solver iterations and final support pass. Software Vulkan only, not hardware-GPU capacity evidence.

| Workload | Bodies | Substeps | Submit p50 ms | Wait/readback p50 ms | Batch p50 ms | Batch p95 ms | Batch p99 ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| Backend sparse | 32 | 2 | 0.0647 | 0.9759 | 1.0426 | 1.5343 | 1.8695 |
| Backend sparse | 128 | 2 | 0.0723 | 1.2124 | 1.2860 | 1.8705 | 2.1106 |
| Backend sparse | 512 | 2 | 0.0795 | 1.6434 | 1.7263 | 2.7988 | 3.0091 |
| Backend sparse | 1,024 | 2 | 0.0849 | 2.6454 | 2.7495 | 3.7323 | 4.0486 |
| Server-batch sparse | 128 | 6 | 0.1879 | 3.5337 | 3.7337 | 4.5300 | 5.0083 |
| Server-batch pile | 128 | 6 | 0.1904 | 6.3773 | 6.5753 | 7.8756 | 8.7404 |
| Server-batch pile, 16 players | 128 | 6 | 0.1924 | 6.8466 | 7.0557 | 7.9812 | 9.3866 |

The previous single-pass software measurement was 1.04 ms sparse / 1.38 ms piled p95 for six-substep batches. Extra grid rebuilds, contacts and support improve stack behavior at measurable GPU execution cost, without extra synchronization or snapshot bytes. Hardware profiling is needed before choosing production iteration budgets.

The earlier verification covered one layout test and five adapter tests.
