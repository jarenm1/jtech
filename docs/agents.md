# Enemy framework (actors)

Enemies are server-owned actors driven by `CharacterIntent` — the same action
type the character motor consumes for players. A *brain* decides each actor's
intent: a scripted heuristic, a learned policy through the external seam, or
(eventually) an embedded policy. Swapping brain kinds never changes the tick.

## Architecture

```
Species data (ActorKind) ──► Actor { kind, brain, intent, state, health }
        │                          │
        │                  brain.act(obs) or set_actor_intent
        │                          ▼
        │               CharacterIntent (bounded, capability-gated)
        │                          ▼
        └──── profile/body ── step_character ── SimEvent stream
```

Everything per-tick — motor, combat, events, observations — is native Rust.
Species are **data**: `ActorKind` values produced in Rust today, loadable from
game packages later.

## The seam (`crates/simulation`)

| Surface | Purpose |
| --- | --- |
| `spawn_actor_kind(kind, position)` | Spawn a species instance |
| `set_actor_intent(id, intent)` | Drive an actor externally (trainers, tests) |
| `observe(id, &world)` | Flat `ActorObservation` — the policy's view |
| `drain_events()` | `SimEvent`s (damage + deaths) with attacker/victim attribution — bounded backlog, oldest drop past 4096 undrained |
| `ActorKind::{dummy, titan, decoy}` | Built-in species tables |

`ActorObservation` deliberately exposes only what a policy may see: own
kinematic state, own health/cooldown, and the nearest living player/actor with
a terrain-occlusion `line_of_sight` flag. Scripted brains consume the same
struct, so recorded heuristic play is valid imitation data for later
behavior-cloning warm starts.

`CharacterIntent` is the shared action contract. `ActorCapabilities` gates
fields a species lacks (e.g. `ITEM` for equipment users); gated fields are
zeroed before the motor — one fixed action shape, per-species dims at the
environment layer.

## Species

| Kind | Locomotion | Capabilities | Combat |
| --- | --- | --- | --- |
| `dummy` | Ground | all | `MELEE_HANDS`, brain `Idle` |
| `titan` | Ground, heavy body/profile | `MELEE` | Innate large-arc `MeleeSpec`, brain `Hunter` |
| `decoy` | Ground, light | `JUMP` | None, brain `Flee` (training prey) |

New species = a table entry + optionally a `Brain`. No motor changes unless a
species needs a new `LocomotionMode` (fly/swim — deferred).

At startup the server spawns a `dummy` at spawn+4 and a `titan` at spawn+24
(inside the eagerly generated spawn chunks). `ServerConfig::spawn_titan`
(`server --no-titan`) drops the hunter for deterministic tests/smoke.


## Determinism notes

- Actor iteration is `BTreeMap`-ordered; combat damage events record actual
  applied amounts with attribution.
- Still not bitwise deterministic: float motor, player `HashMap` order,
  async terrain/GPU paths. Sufficient for RL; replay-grade determinism needs
  the remaining orderings fixed.

## Deferred work (intentionally not built)

- **Locomotion modes** — `LocomotionMode { Ground, Fly, Swim, Climb }` enum +
  per-kind transition data; `climb`/`pitch` intent fields. Required by flying
  species (bat). `noclip` already demonstrates the fly pattern.
- **Ranged actor attacks** — `attack` routed by kind to the existing
  projectile path instead of `resolve_swing` (bat).
- **Equipment** — `ITEM` actors resolving `held_item` through the melee table,
  actor inventory/loadouts (skeleton, human NPCs).
- **Embedded/online policies** — a third `ActorBrain` variant holding weights;
  weights ship as package assets.
- **`"actor"` package kind** — species + authored brain specs as Scheme data,
  compiled to native at load (same discard-the-VM pattern as melee/terrain).
  Add once ≥3 species exist and brain-tuning churn justifies it.
- **Vec-env / PufferLib binding** — lives in a separate harness crate over
  `SimulationPlugin::headless`; never in `simulation`. Frozen terrain + GPU
  physics off for training episodes.
- **Episode `reset(seed)`** — restore actors/players/terrain-edit state for
  training episodes.
