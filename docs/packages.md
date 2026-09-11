# Gameplay packages

Packages use server-side Steel Scheme. Edit `packages/explosive-bow/server.scm`
to change firing rate, projectile flight, and impact behavior. Rust handles
request validation, swept collision, native blast allocation, world mutation,
and replication. Clients use native visuals and receive the server's firing rate
and package status.

Author terrain in `packages/terrain/server.scm`. At world creation the server
compiles the Scheme terrain graph into an immutable native generator. See
[terrain generation](terrain.md) for the graph and biome API. Restart the server
to apply terrain changes; bow packages support live reload.

## Run and reload

From the repository root:

```sh
cargo run -p server -- --packages ./packages --gpu-physics
cargo run -p voxel-client
```

Use slot 6 for the bow. Save `server.scm` while the server runs. The server checks
for source changes every 250 ms and compiles a candidate in a background thread.
At a simulation tick boundary, it installs the candidate after checking the API,
firing rate, and every power preset's computed flight and blast settings.

Read the top-right **SERVER PACKAGES** panel for loading, reloading, loaded, or
error status. Errors include available Scheme source diagnostics. A failed reload
retains the previous loaded generation; fix the source and save again to retry.
A missing or invalid package at startup disables the bow until a valid file loads.
The server also prints lifecycle changes to stderr.

New shots use the newly installed generation. In-flight arrows retain their
original flight and blast settings. Already queued
blasts retain their validated parameters. A firing-rate change resets client and
server cooldown deadlines.

The server loads the initial package during startup. By default, it searches for
`packages` beside the executable, then in the working directory, with a source-tree
fallback for development. Use `--packages DIR` for an explicit deployment path;
ship that directory with the server executable.

## Server package API v1

Define these exports in `explosive-bow/server.scm`:

| Export | Contract |
| --- | --- |
| `package-api-version` | Integer `1` |
| `shots-per-second` | Integer from 1 through 60 |
| `(projectile-for-power power)` | Return `(projectile speed gravity travel lifetime)` |
| `(blast-for-power power)` | Return `(explosion radius energy player-speed absorbed pulse)` |

Power is an integer from 0 through 3, corresponding to the four existing bow
presets. The loader evaluates these functions for all presets, validates their
results, then discards the VM. Simulation ticks read the resulting immutable
native policy tables. Use Scheme expressions, helper functions, and macros to
author those policies. Live event callbacks and world access are later API work.

`projectile` and `explosion` are host-provided Scheme constructors returning lists.
All numeric outputs must be finite. Host validation applies these bounds:

| Field | Units | Range |
| --- | --- | --- |
| speed | m/s | 0.1–200 |
| gravity | m/s² | 0–100 |
| travel | m | 0.1–256 |
| lifetime | fixed60 ticks | integer 1–3600 |
| radius | m | 0.1–12 |
| energy | material blast joules | 0–100000 |
| player-speed | unobstructed center launch speed, m/s | 0–100 |
| absorbed | dissipated fraction of material energy | 0–1 |
| pulse | pressure pulse seconds | 0.00001–1 |

The client sends an aim and preset, never these authored physical values. The
server uses the same existing projectile/explosion messages for presentation;
protocol version 10 adds reliable package lifecycle messages.

## Execution boundary

Install only trusted local packages for this first implementation. The VM uses
Steel's sandbox configuration, source files are limited to 64 KiB, and a watchdog
requests cooperative interruption after 5 seconds of source evaluation or 20 ms
per policy function during loading.
Those limits are not a hard memory or process-security boundary; native/compiler
work may not respond promptly to interruption. A reload still pending after six
seconds reports an error. If the loader never returns, restart the server after
fixing the source. Only one candidate loader runs at a time.

Bow API v1 covers one server weapon slot with one watched entry file. Terrain
has a separate, startup-only API and entry file. Client-side Scheme, dependency
resolution, and package distribution require subsequent API work.
