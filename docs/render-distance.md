# Render distance and stress testing

The default horizontal radius is **16 chunks (512 m)**. The square covers
33 × 33 columns. Vertical interest follows terrain surfaces and cliff walls,
with a local volume around each player for flight and digging. Edited chunks
and their neighbors are included even above or below the natural terrain.
A joining connection is placed only on loaded terrain with solid support and
clear headroom, and is rejected when no such cell is available.
World height is **-256 through 767 blocks**; the full vertical range is not
loaded for every column. See [terrain generation](terrain.md).

Use `--radius` on the server to select 1–64 chunks. Restart the client and server
after rebuilding. The client camera covers the full diagonal at the maximum
radius; ordinary frustum culling still applies.

| Server radius | Horizontal reach | Chunk columns |
| --- | --- | --- |
| 16 (default) | 512 m | 1,089 |
| 32 | 1,024 m | 4,225 |
| 64 | 2,048 m | 16,641 |

Chunk counts depend on relief, player elevation, and construction.

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

A native background worker surveys column height bounds and generates chunks. At
most 16 requests are outstanding; chunk requests are capped at 12 so at least 4
slots stay reserved for surveys, with up to 8 new chunk requests and 8 surveys
submitted per tick when capacity permits. A survey returns the exact minimum and
maximum surface height for one chunk column, and visible interest takes the
minimum of each column and its four neighbours so exposed cliff walls stay
loaded. Completed terrain is installed at tick boundaries, never replaces a
chunk that is already resident, and then replays the latest journaled edits.
Physics collision halos have generation priority. Streaming sends up to **8
chunks per player per tick**, nearest first.
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
