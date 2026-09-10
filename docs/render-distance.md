# Render distance and stress testing

The default horizontal radius is **16 chunks (512 m)**, up from 3 chunks (96 m).
The square covers 33 × 33 columns and three vertical layers: **3,267 chunks**,
about **22 times** the old footprint. Load terrain progressively, nearest first.

Use `--radius` on the server to select 1–64 chunks. Restart the client and server
after rebuilding. The client camera covers the full diagonal at the maximum
radius; ordinary frustum culling still applies.

| Server radius | Horizontal reach | Chunk columns | Chunks |
| --- | --- | --- | --- |
| 16 (default) | 512 m | 1,089 | 3,267 |
| 32 | 1,024 m | 4,225 | 12,675 |
| 64 | 2,048 m | 16,641 | 49,923 |

For a high-load run:

```sh
cargo run --release -p server -- --gpu-physics --radius 64 --metrics-every 120
cargo run --release -p voxel-client
```

Start at 16, then compare 32 and 64 from the same position. Fly upward with **V**
and **Space** to expose more terrain. Compare client FPS/frame times, rendered
chunks, triangles and upload totals with server tick times and chunk counts.
Wait for streaming and meshing to finish before comparing steady-state frames.
At 64, terrain storage, mesh memory and draw submission are intentional stressors.

Streaming generates up to **8 chunks per server tick** and sends up to **8 per
player per tick**. Choose the next batch with linear-time partitioning instead
of sorting the whole interest set. Physics collision halos have priority.
Meshing allows **16 jobs**, up to **8 starts and uploads per frame**, and an
**8 MiB upload budget**. Large single meshes may upload alone. GPU collision
residency follows loose-body halos rather than the render radius.

Run the headless server streaming benchmark separately from graphical testing:

```sh
cargo test -p simulation large_radius_streaming_benchmark -- --ignored --nocapture
```

This fills the complete interest and safety regions at radii 3, 16 and 64,
checks baseline delivery bookkeeping, and prints server timings. It excludes
socket transport, client meshing, rendering and GPU physics; measure FPS in the
running client.
