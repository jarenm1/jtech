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

Aim at a highlighted grid block and click **M1** repeatedly. Watch its fracture percentage in the HUD; at 100%, the block is destroyed. Use **right click** to place blocks. Near spawn, press **F** for a debug launch. A chipped block carries its damage while loose and after settling into another grid cell. Try launching a partly damaged block into terrain to finish breaking it.

Rebuild and restart both processes: protocol version **4** includes hit progress and typed rejection reasons. Connect a second client to observe authoritative destruction, loose-body motion, and settlement.

For player contacts, launch stone and wood blocks and walk against them. With the same walking force, the heavier stone accelerates more slowly. Jump onto a loose block, stand on it, and jump off. Moving blocks can displace the character; terrain clearance takes priority when the character is trapped.

## Implemented scope

The server admits 128 loose blocks inside one collision region, `x,z = -16..16` and `y = -8..40` (upper bounds exclusive). Strike targets must be one cell inside its boundary. Stay near spawn for this slice. Outside the collision region, the GPU treats terrain as solid.

The GPU simulates axis-aligned unit cubes, gravity, terrain and block contacts, friction, and floating-point fracture damage. A width-two spatial grid and four contact iterations per substep support short stacks. Material constants are authored once in `crates/gpu_physics/src/lib.rs`; generate shader constants from that table.

Players use the shared CPU swept-AABB character controller on server and client. Walking against loose blocks supplies a **60 N budget per player per substep**, shared across touched bodies and applied once before contact iterations. Position overlap alone does not push a loose body. Incoming body impacts use a kinematic character boundary; this is a force-limited walking motor, not a symmetric player rigid-body solver. Body observations arrive at 20 Hz.

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

Initial terrain upload is 192 KiB; subsequent edits use coalesced 4-byte cell patches. Body buffers persist between batches and are rewritten only for lifecycle changes. If a batch is late, skip submission rather than accumulate work; physics slows under overload. Metrics include skipped periods, transfer bytes, and observed completion latency.

Upload at most sixteen 32-byte player collider records per active batch (512 bytes, 10 KiB/s at 20 Hz). No extra GPU readback or per-player GPU wait is needed. Network body snapshots include velocity, adding 12 bytes per body before encoding. CPU player collisions use completed authoritative poses; rendering interpolation does not feed back into simulation.

Replicate only changed observations/lifecycle state, at most 20 Hz, plus an initial snapshot for new clients. The physics message `tick` is an observation revision, not the player simulation's wall tick. Placement requests in the collision region are rejected while GPU work is pending to avoid using stale positions for overlap checks.

CPU readback is needed for replication and grid transactions. Measure it separately from shader execution. For the full system, decouple the physics tick from the replication cadence and compact settling/damage events on the GPU. Avoid reading every body every tick solely to discover that almost nothing changed.

## Larger architecture

1. **Sparse active regions.** Maintain a GPU page table for resident terrain chunks, coarse occupancy masks for empty-space rejection, and an explicit unknown-terrain policy. Use region-relative coordinates for large worlds.
2. **Spatial broadphase.** Index dynamic swept bounds with a sorted spatial grid. Account for duplicate cell entries, pair deduplication, region borders, and dense-contact overflow. Keep the static voxel grid as its own collision index.
3. **Fracture and loading.** Add explicit face bonds, structural support propagation, and energy-budgeted explosions through the contact response. Add rotation and contact torque when expanding beyond unit-cube translation.
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
cargo run --release -p gpu_physics --example benchmark
```

See the [2026-09-10 backend measurements](../crates/gpu_physics/README.md#software-adapter-measurement-2026-09-10). With this model, 128 piled bodies and sixteen player inputs measured **10.76 ms p95** per six-substep batch including completion/readback on llvmpipe. Median CPU submission was **0.326 ms**. All **52** available workspace tests passed, including software-GPU execution; socket-dependent networking was excluded. Clippy and formatting passed.

In a restricted environment, run CPU checks with `TMPDIR=/tmp cargo test --workspace --exclude networking`. Select software Vulkan with `VK_ICD_FILENAMES=/run/opengl-driver/share/vulkan/icd.d/lvp_icd.x86_64.json` for adapter tests where that ICD is installed. The smoke harness requires loopback socket access; desktop acceptance requires compositor access.
