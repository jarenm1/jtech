# Gameplay packages

Packages use server-side Steel Scheme. Every subdirectory of the packages root
(except `terrain`, which has a separate startup-only API) is a live-reloadable
package slot: edit its `server.scm` and the server compiles a candidate in a
background thread, then installs it at a simulation tick boundary. Rust handles
request validation, swept collision, native blast allocation, world mutation,
and replication. Clients use native visuals and receive package status plus the
replicated launcher and melee item tables.

Each `server.scm` declares `(define package-api-version 1)` and a
`(define package-kind "...")`. Known kinds:

| Kind | Native result |
| --- | --- |
| `"launcher"` | `LauncherPackage`: one claimed item id, firing rate, and authored power presets |
| `"melee"` | `MeleePackage`: authored weapons merged into the shared melee table, plus a spawn loadout |

Item ids are a shared namespace across packages: ids below 6 are hands and
block materials, and launchers and melee weapons claim the rest. If two loaded
packages claim the same id, the later one in sorted directory order reports an
error and its items stay inactive until the conflict is fixed. This is how a
future package (say, a flame sword or a second launcher) composes with the
base set: it registers its own ids and they merge into the same tables, swing
and shot admission, and replication — no host changes needed.

Author terrain in `packages/terrain/server.scm`. At world creation the server
compiles the Scheme terrain graph into an immutable native generator. See
[terrain generation](terrain.md) for the graph and biome API. Restart the server
to apply terrain changes; runtime packages support live reload.

## Run and reload

From the repository root:

```sh
cargo run -p server -- --packages ./packages --gpu-physics
cargo run -p voxel-client
```

Launcher items appear in the hotbar once packages load; melee weapons are
ordinary inventory items you can hotbar. Save any `server.scm` while the
server runs. The server checks for source changes and new or removed package
directories every 250 ms and compiles candidates in background threads, one
per package. At a simulation tick boundary, it installs each candidate after
checking the API and every authored value.

Read the top-right **SERVER PACKAGES** panel for per-package loading,
reloading, loaded, or error status. Errors include available Scheme source
diagnostics. A failed reload retains the previous loaded generation; fix the
source and save again to retry. A missing or invalid package at startup leaves
its slot in error until a valid file loads. The server also prints lifecycle
changes to stderr.

New shots and swings use the newly installed generation. In-flight projectiles
retain their original flight and blast settings; already queued blasts retain
their validated parameters. A firing-rate change resets client and server
cooldown deadlines.

The server loads the initial packages during startup. By default, it searches
for `packages` beside the executable, then in the working directory, with a
source-tree fallback for development. Use `--packages DIR` for an explicit
deployment path; ship that directory with the server executable.

## Server package API v1

Every package exports `package-api-version` (integer `1`) and `package-kind`
(string). Kind-specific exports:

### `"launcher"` — `explosive-bow/server.scm`

| Export | Contract |
| --- | --- |
| `launcher-item` | Equipment item id, integer in 6–255, unique across packages |
| `launcher-name` | Display string replicated to clients |
| `shots-per-second` | Integer from 1 through 60 |
| `powers` | 1–8 `(launcher-power label projectile explosion)` presets, in client cycle order |

`label` is the display text for the preset; `projectile` and `explosion` are
host-provided constructors. The client cycles `powers` and sends an index;
each preset's projectile and blast apply when its shot fires and impacts. The
loader evaluates and validates the whole `powers` list, then discards the VM;
simulation ticks read the resulting immutable native policy table. Use Scheme
expressions, helper functions, and macros to author those policies. Live event
callbacks and world access are later API work.

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
server uses the same existing projectile/explosion messages for presentation.

### `"melee"` — `melee/server.scm`

| Export | Contract |
| --- | --- |
| `weapons` | List of `(melee-weapon id name range damage cooldown-ticks knockback [model])` |
| `spawn-items` | Optional list of `(id count)` pairs granted on spawn and respawn |

`melee-weapon` is a host-provided constructor. `id` is an integer of 6 or above
up to 2³²−1 and unique across loaded packages; `name` is a display string replicated to
clients; `range` is metres 0.1–16; `damage` is whole health points 1–65535;
`cooldown-ticks` is fixed60 ticks 0–600; `knockback` is kg·m/s 0–10000.
Head-zone hits double damage; unarmed hands stay the fallback for unregistered
items. `spawn-items` entries must reference ids the same package registers.
The optional `model` is a `.glb` path under the package's `assets/` directory;
see [Package assets](#package-assets).

Weapons are equipment, not stacks: every copy occupies its own inventory slot,
so picking up a duplicate adds a second entry rather than merging or being
refused — future per-item meta needs the slot. A death drops each copy singly,
and clients show no stack count. A player may only swing a registered weapon
they carry; selecting an unowned weapon refuses the swing entirely rather than
downgrading to hands. Weapon specs, names, and item kind replicate in the
package message so clients route swings and display authored names without
hardcoding.

## Package assets

Files under a package's `assets/` directory ship to clients. The host hashes
them into a manifest carried in the Packages message; clients request missing
files, receive them in reliable 64 KiB chunks, and expose them to Bevy through
a `pkg://` asset source backed by a content-addressed cache in the temp dir.
Files are capped at 16 MiB and only served while their package is loaded.
Editing an asset bumps the manifest hash, so clients re-download on the next
sync without a restart.

The startup-only `terrain` package has no live slot but ships its scatter
models the same way: files under `packages/terrain/assets/` are always in the
manifest, and the species table in the Welcome message names them. See
[terrain generation](terrain.md#organic-scatter).

A melee weapon's optional trailing `model` argument names a `.glb` under
`assets/`; the loader fails the package if the file is missing, and clients
render the model for dropped copies (a colored cube until the download lands,
a neutral cube for weapons without a model). Models only affect drop
presentation — hotbar tiles and held items still use swatches.

## Execution boundary

Install only trusted local packages for this first implementation. The VM uses
Steel's sandbox configuration, source files are limited to 64 KiB, and a
watchdog requests cooperative interruption after 5 seconds of source evaluation
during loading. Those limits are not a hard memory or process-security boundary;
native/compiler work may not respond promptly to interruption. A reload still
pending after six seconds reports an error. If the loader never returns,
restart the server after fixing the source. Each package
slot runs at most one candidate loader at a time.

API v1 covers compile-time policy: weapon tables, firing rates, and spawn
loadouts. Terrain has a separate, startup-only API and entry file. Live event
callbacks (on-hit effects like a flame sword's burn), world access, client-side
Scheme, dependency resolution, and package distribution require subsequent API
work.
