# GPU voxel physics

## Run the slice

Start the authoritative server with GPU physics enabled:

```sh
cargo run --release -p server -- --gpu-physics
```

In another terminal, open the client:

```sh
cargo run --release -p voxel-client
```

Aim at a grid block under the center-screen crosshair and click **M1** repeatedly to fracture it. Use **1–5** and **right click** to place blocks. Press **6** to equip the **Explosive Bow**, then hold **right click** to fire explosive arrows at **25 shots per second** with the default package. Aim beyond block-reach or at loose blocks. Press **F** for a debug launch. A chipped block carries its damage while loose and after settling into another grid cell.

Rebuild and restart both processes for protocol version **9**, including bow power presets and player blast momentum. Press **V** to toggle flight, **Space** to rise, and **Ctrl** to descend. Connect a second client to observe authoritative arrows, explosions, destruction, loose-body motion, and settlement.

For player contacts, launch stone and wood blocks and walk against them. With the same walking force, the heavier stone accelerates more slowly. Jump onto a loose block, stand on it, and jump off. Moving blocks can displace the character; terrain clearance takes priority when the character is trapped.

## Implemented scope

The server admits **128 loose blocks globally**, at any loaded horizontal location within the world's coordinate bounds. Multiple distant groups share one GPU solver. Terrain spans chunk Y **−1 through 1**, with the bottom cell layer protected. Body IDs and damage follow blocks across chunk boundaries.

The GPU simulates axis-aligned unit cubes, gravity, terrain and block contacts, friction, and floating-point fracture damage. A world-space width-two **hashed spatial grid** and four contact iterations per substep support short stacks. Filter bucket candidates by their exact cell before resolving contacts, so hash collisions cannot apply a contact twice. Material constants are authored once in `crates/gpu_physics/src/lib.rs`; generate shader constants from that table.

Players use the shared CPU swept-AABB motor in the `controller` crate on server and client. See [Character controllers](controller.md) for intent, body/profile configuration, Bevy scheduling, and vision-policy adapters. Walking against loose blocks supplies a **60 N budget per player per substep**, shared across touched bodies and applied once before contact iterations. Position overlap alone does not push a loose body. Incoming body impacts use a kinematic character boundary; this is a force-limited walking motor, not a symmetric player rigid-body solver. Body observations arrive at 20 Hz.

## Explosive bow

Test arrows are unlimited, with a **0.04-second** successful-shot interval (**25 shots per second**). At fixed60, carry fractional-tick timing forward using alternating two- and three-tick gaps. Hold right click for repeat fire; release it to stop. The server derives the muzzle from the player's eye and validates aim, loaded air, request order, and capacity. Replayed requests return their cached result. Arrows travel at **36 m/s** under **3 m/s²** gravity, sweep against terrain and loose unit cubes, and detonate on the first impact. They expire after **100 km** of travel or **5 minutes**; leaving loaded terrain does not expire them, so a shot straight up falls back into the world. Admit at most **32** flying arrows and queued detonations combined; process at most **two explosions per tick**.

**Left click** is the general attack action. With the explosive bow (6, an admin item) it fires immediately on the press. With the plain Bow (8) it draws: hold to build charge over **1 second**, slowing movement toward **0.15x**, with a charge bar under the crosshair and the held-item animation spanning the draw. At full charge the arrow holds ready; release to fire, or release early to cancel. The motor owns the draw, so the server fires on the same edge and the shot cannot be skipped. Each arrow keeps the power it was fired with, including through delayed physics readback and queued detonations.

| Power | Charge | Radius |
| --- | --- | --- |
| 0.5x | 3,000 J | 3 m |
| 1x | 6,000 J | 4 m |
| 2x | 12,000 J | 5 m |
| 4x | 24,000 J | 6 m |

Each charge allocates **35% absorbed work** and **65% reserved kinetic energy**. Sample facing cube-face centers for exposure against terrain and loose blocks before editing any receivers. Weight by `(1 - distance / radius)² / (distance² + 0.25)` using cube-center distance, and normalize by at least one. Occluded blocks receive no share; low-coverage energy is lost to the air. The protected bottom layer is excluded.

Estimate material load from the reserved rest-mass impulse over a **0.75 ms** pulse and **1 m²** effective area. The short pulse and motion-heavy energy split provide a wider band of surviving attachment failures around central destruction. Apply absorbed work through material pressure gating and attachment strength. Spend reserved material kinetic energy on a successful immediate detachment or a surviving loose body. Calculate impulse from current velocity plus queued momentum, so successive blasts add only their assigned change in kinetic energy. Unspent launch energy is discarded. GPU-disabled servers apply terrain damage and player knockback; loose-block launch requires `--gpu-physics`.

For released terrain, use an authored crater-ejection direction: normalize the radial direction plus twice the exposed-surface normal. Weight visible face normals by the corresponding absolute component of the center offset, then normalize their sum. This directs floor fragments up and wall fragments out into air rather than back into their support. Compute the impulse magnitude from the reserved kinetic budget after choosing direction. Existing loose bodies receive radial impulses.

Player knockback uses a separate per-actor gameplay budget, independent of debris count and other players. For an **80 kg** player, use `0.5 * mass * 18² * power_multiplier * exposure * (1 - distance / radius)²` joules of launch energy, then compute the radial impulse from current velocity. This gives a resting player up to **18 m/s** at standard power before falloff. Sample three heights above the feet against pre-blast terrain and loose bodies; use the exposed fraction and torso-center distance. Apply once per queued detonation, including to the shooter. In the flat stone-floor fixture, standard power lifts the player about **2.35 m** when aiming steeply down, or **0.48 m** at roughly 45°. Higher presets give stronger launches.

Replicate horizontal external velocity separately from the walking motor so input replay preserves momentum. Apply drag and collision response in the shared CPU controller; noclip ignores blasts. Player launch also works without GPU physics.

The flat, two-layer surface fixture yields **16–24** surviving attachment failures across the five materials, with central destruction in every case. The stone-floor GPU test exercises terrain patches and simultaneous releases; after **0.25 seconds**, **27** surviving blocks moved and **17** rose more than **0.5 m**. These are fixture results, not guaranteed counts for arbitrary geometry.

Defer arrow collision advancement while a GPU batch is pending, then advance at most **50 ms** using fresh body poses. Lifetime continues during stalls. Apply blast body damage and impulses while idle, clear settlement readiness, and upload before the next batch. Replicate arrow observations at **20 Hz**; interpolate visible shafts and tips and display a **450 ms** expanding blast effect on clients.

Repeated explosions include newly detached and already moving bodies as receivers. Accumulate fracture damage, add impulses by stable body ID, and remove destroyed bodies and their pending loads together. Queue impacts during an in-flight batch; evaluate each detonation against the latest completed positions and the ownership changes of earlier detonations. A survivor hit before submission receives the combined reserved impulse once.

## Active terrain and reusable kernels

`active_terrain.rs` computes a **16 m collision halo per body**, independent of player visibility. The halo covers one 50 ms batch: at most 1.5 m of integration, 12 m of bounded contact projections, and the half-metre cube extent. Pin these chunks and detached source cells for recovery. Prioritize missing physics chunks in the eight-chunks-per-tick world generation queue. Postpone submission until the collision halo is loaded, preserving queued loads; track these waits separately from GPU backpressure.

Upload only new or changed chunk snapshots. Evict GPU pages when bodies no longer need them; retain stable body identities and velocities as the page set changes. Source cells remain pinned until their bodies settle or are destroyed. World chunk eviction uses the ordinary idle timeout once neither players nor physics need a chunk.

The backend accepts up to **4,096 packed `32³` terrain pages** in one reusable sparse buffer, with eight material IDs per word. Page allocation and validation are CPU-side; shared WGSL terrain lookup handles collision reads. Missing pages are conservative solid boundaries and cannot emit terrain-damage events. Separate terrain lookup, hash-grid construction, integration, contact response, and finalization from weapon-specific blast planning. Dense terrain constructors are available for isolated solver fixtures and benchmarks.

Use `SimulationPlugin::headless(config)` to run the authoritative update and ownership paths without network sockets. The network server uses the same initialization and simulation methods.

## Material and attachment model

Treat a voxel as one cubic metre. Mass is density times volume. The densities are gameplay values, not measurements of real materials.

| Material | Density kg/m³ | Damage onset Pa | Fracture budget J/m³ | Attachment strength N/m² | M1 hits |
|---|---:|---:|---:|---:|---:|
| Grass | 1.0 | 1,000 | 24 | 1,000 | 4 |
| Dirt | 1.5 | 1,200 | 36 | 1,200 | 6 |
| Stone | 3.0 | 2,000 | 60 | 1,500 | 10 |
| Sand | 1.2 | 800 | 18 | 1,000 | 3 |
| Wood | 0.7 | 1,500 | 90 | 1,200 | 15 |

For a contact, divide its normal impulse by a fixed 1/120-second window to estimate load, and divide load by contact area to estimate pressure. The damaging fraction follows smoothstep from zero at the material onset to one at twice the onset. Multiply that fraction by the material's share of dissipated normal energy and its fracture efficiency (currently 0.5). Accumulate the result without decay. Energy retained as rebound or motion does not count as dissipated work. Sliding friction is damping in this slice, not abrasion damage.

Split dissipated work equally between the two participants. For a body face overlapping several terrain voxels, distribute that work and load by overlapping area. Support and gentle broad pushing fall below damage onset. The Jacobi pile solver is an approximation, not a globally energy-conserving fracture solver.

M1 is an authored inelastic tool contact: **24 J**, effective mass **2 kg**, duration **25 ms**, area **0.01 m²**. Half its dissipated work enters the target, so a fully concentrated strike adds **6 J** of fracture damage. The grid resists its approximately 392 N load while material damage accumulates.

Attachment area is one square metre per occupied neighboring face. Compare each observed contact load with strength times attachment area. Load does not accumulate across strikes. Once an attachment fails, latch a pending release and retry the ownership transaction when GPU state and body capacity permit it. Release at rest: the collision impulse was already reacted into the grid and must not be spent again. F explicitly releases the attachment and injects debug momentum.

Store up to **65,536** damaged/pending-release grid exceptions on the server. Preserve them across chunk eviction within the running session. Transfer joules into a body on detachment and back to a grid exception on settlement. Clear the exception when replacing or destroying its voxel. Route grid and loose destruction through `Simulation::destroyed` for future item spawning. Damage is session state, like the current edit journal; disk save/load is future work.

Apply completed GPU damage before accepting new edits. Discard contact observations for voxels changed while that batch was pending. M1 partial hits leave voxel revisions unchanged. Cache request results so replaying a hit does not apply damage twice. Queue up to four F launches per player for at most one second when GPU work is pending; revalidate revision, ray, reach, capacity, and cooldown before applying. Rejected requests do not restart the successful-action cooldown.

On a reported backend failure, stop admitting bodies and retry restoration into each detached block's original cell. Retain a body if that cell is occupied or restoration would intersect a player or another body. A failed backend requires a server restart before accepting new strikes.

## Data ownership

- Store terrain in the existing uniform or 4-bit `32³` chunks.
- Derive position from chunk coordinates and local indices. Share material properties rather than copying them onto static voxels.
- Store moving unit cubes in GPU-resident body buffers. Allocate contact, spatial-index, and readback buffers separately from the body representation.
- Use Bevy resources and systems to manage the backend and transactions. Use ECS entities for client presentation and gameplay objects that need them.
- Validate player requests on the server. Clients send targets, not arbitrary force or velocity values.
- Commit detachment and settlement through authoritative voxel revisions and the edit journal.

The client interpolates body snapshots. Do not require identical client and server GPU arithmetic for authority or reconciliation.

## Synchronization budget

Batch six 120 Hz substeps per submission at 20 Hz, with one asynchronous readback in flight. A full 128-body copy is **6,144 bytes** (48 bytes/body). Copy a fixed terrain-event buffer of **262,160 bytes** with the same submission: up to 8,192 padded 32-byte contacts plus a header. Total payload is about **5.12 MiB/s** at 20 Hz and full body count. The bounded event stream adds bandwidth rather than a second GPU wait. Record event overflow explicitly; excess terrain events are dropped, so monitor overflow before increasing pile size or substeps.

A packed terrain page is **16 KiB**; page-table changes are uploaded separately within the same terrain buffer. Unchanged chunk snapshots require no terrain upload. Body buffers persist between batches and are rewritten for lifecycle changes and authoritative external loads. If a batch is late or its terrain is not ready, postpone submission rather than accumulate work. Metrics include resident chunks, terrain waits, skipped GPU periods, transfer bytes, and observed completion latency.

Upload at most sixteen 32-byte player collider records per active batch (512 bytes, 10 KiB/s at 20 Hz). No extra GPU readback or per-player GPU wait is needed. Network body snapshots include velocity, adding 12 bytes per body before encoding. CPU player collisions use completed authoritative poses; rendering interpolation does not feed back into simulation.

Replicate only changed observations/lifecycle state, at most 20 Hz, plus an initial snapshot for new clients. The physics message `tick` is an observation revision, not the player simulation's wall tick. Reject placements inside active collision halos while GPU work is pending to avoid stale overlap checks. Apply completed terrain contacts only when the authoritative cell still matches and was not edited during that batch.

CPU readback is needed for replication and grid transactions. Measure it separately from shader execution. For the full system, decouple the physics tick from the replication cadence and compact settling/damage events on the GPU. Avoid reading every body every tick solely to discover that almost nothing changed.

## Larger architecture

1. **Terrain acceleration.** Add coarse occupancy masks for empty-space rejection and region-relative floating-point coordinates for extreme world distances.
2. **Spatial broadphase.** Measure hash occupancy and candidate traversal before increasing body capacity. Use swept bounds and wider neighborhoods if expanding beyond unit cubes and bounded substeps.
3. **Fracture and loading.** Add explicit face bonds, structural support propagation, and finer blast exposure sampling. Add rotation and contact torque when expanding beyond unit-cube translation.
4. **Sparse damage.** Organize exceptions per chunk and switch to dense storage where damage density warrants it. Add disk persistence with the world journal.
5. **Transactional settlement.** Require low motion and support, resolve competing cell claims in stable order, exclude players and bodies, and update dynamic ownership and terrain together. Retry failed placement rather than duplicating or discarding a block.
6. **Replication.** Send relevant body snapshots and reliable lifecycle/world events. Use stable handles with generations when recycling slots. Separate authoritative bodies from cosmetic debris.
7. **Capacity.** Set budgets for active bodies, contacts, fracture propagation, uploads, readback, and network traffic. Define overload behavior before increasing capacity. Keep sleeping regions out of active work.

## Performance acceptance

The target of 1,000 interacting players requires both distributed and concentrated load tests. Measure active bodies, candidate pairs, solved contacts, terrain changes, network bytes, and p50/p95/p99 whole-tick latency on a named server GPU. A 60 Hz server has 16.67 ms for the entire tick. Kernel timing alone cannot establish server capacity.

The current multiplayer protocol admits 16 players. This slice is a bounded foundation, not a 1,000-player capacity claim. Rotation, connected multiblock rigid bodies, and structural support propagation require subsequent work.

## Checks and measurements

```sh
cargo test --workspace
cargo test -p gpu_physics --lib -- --include-ignored
cargo test -p simulation --lib -- --include-ignored
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p server --bin smoke
cargo run --release -p gpu_physics --example benchmark -- --sparse-terrain
```

See the [2026-09-10 sparse-backend measurements](../crates/gpu_physics/README.md#software-adapter-measurement-2026-09-10). With packed terrain and the hashed broadphase, 128 piled bodies and sixteen player inputs measured **12.87 ms p95** per six-substep batch including completion/readback on llvmpipe. Median CPU submission was **0.307 ms**. These timings exclude terrain streaming and surrounding server work. **81 workspace tests** passed with software Vulkan, excluding socket-dependent networking. Coverage includes simultaneous distant explosions, playerless halo loading and chunk crossing, remote settlement/eviction, and repeated-blast damage and impulse routing through slot compaction, alongside bow, exposure, energy-budget, and client lifecycle checks.

In a restricted environment, run CPU checks with `TMPDIR=/tmp cargo test --workspace --exclude networking`. Select software Vulkan with `VK_ICD_FILENAMES=/run/opengl-driver/share/vulkan/icd.d/lvp_icd.x86_64.json` for adapter tests where that ICD is installed. The smoke harness requires loopback socket access; desktop acceptance requires compositor access.
