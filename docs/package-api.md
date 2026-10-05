# Package API: current surface and modding gaps

A map of what a package can author today, where the hard edges are, and what a
modder would reach for next. `docs/packages.md` is the reference; this is the
analysis that drives API v2.

## 1. The current surface

### Lifecycle

One directory per package under the packages root, one `server.scm` each.
The server polls every 250 ms, compiles a candidate in a background thread,
validates it, and installs it at a tick boundary. A failed reload keeps the
previous generation. `terrain` is the exception: startup-only, separate API.

### Exports

| Export | Kind | Contract |
| --- | --- | --- |
| `package-api-version` | both | integer `1` |
| `package-kind` | both | `"bow"` or `"melee"` |
| `shots-per-second` | bow | integer 1–60 |
| `(projectile-for-power power)` | bow | `(projectile speed gravity travel lifetime)` |
| `(blast-for-power power)` | bow | `(explosion radius energy player-speed absorbed pulse)` |
| `weapons` | melee | list of `melee-weapon` / `ranged-weapon` |
| `spawn-items` | melee | optional `(id count)` pairs |
| `sounds` | both | optional `(event path)` pairs |

### Constructors (host-provided Scheme)

```scheme
(projectile speed gravity travel lifetime)
(explosion radius energy player-speed absorbed pulse)
(melee-weapon id name range damage cooldown-ticks knockback [model])
(ranged-weapon id name range damage cooldown-ticks knockback speed gravity travel lifetime [model])
```

### What the host does with it

- **Weapons** merge into one shared `MeleeTable` keyed by item id. The host
  derives the attack kind (`Melee`/`Ranged`), the cooldown, the charge length,
  the projectile spec, the model path, and the display name.
- **The bow** becomes an immutable policy table: four projectile specs, four
  blast specs, one firing rate.
- **Sounds** replicate as `(package, event, path)` and the client plays them.
- **Assets** under `assets/` hash into a manifest; clients download and resolve
  them through `pkg://`.

### Execution boundary

Steel's sandbox, 64 KiB source cap, 5 s source-evaluation watchdog, 20 ms per
policy function, one loader per slot, trusted-local-only. Not a hard security
boundary.

## 2. What's possible today

A modder can, with no host changes:

- **Add melee weapons** — any id above 6, with range, damage, cooldown,
  knockback, and a `.glb` model. They merge into the shared table, so they get
  swing resolution, ownership checks, replication, and drop presentation free.
- **Add ranged weapons** — same, plus a projectile spec; the basic attack fires
  it and it charges before release.
- **Replace the bow** — firing rate and all four power presets (projectile and
  blast).
- **Grant a spawn loadout** — items every player receives on connect and
  respawn.
- **Ship assets** — models and audio, downloaded on demand.
- **Map sounds** onto ten fixed events, including two looping states.
- **Author terrain** — a separate, startup-only graph API.

That is a real modding surface: a weapon pack is entirely data.

## 3. What a modder would want

Grouped by how far the host is from supporting it.

### Close — data the host already has, just not exposed

| Want | Today | Gap |
| --- | --- | --- |
| **Per-weapon sounds** | sounds are per-package | the event table has no weapon key |
| **Per-weapon attack kind** | derived from `ranged` presence | no way to say "beam" or "thrown" |
| **Item display metadata** | name only | no description, icon, rarity, or tooltip |
| **Stackable consumables** | every weapon is `Equipment` | no way to author a potion or a tool |
| **Per-weapon cooldown/charge tuning** | cooldown yes, charge fixed at 60 | charge length is host constant |
| **Weapon stat scaling** | flat damage | no crit, no falloff, no head-zone control |

### Medium — needs a host hook, not a new subsystem

| Want | Today | Gap |
| --- | --- | --- |
| **On-hit effects** (burn, poison, lifesteal) | none | no event callback; the docs already list this as v2 |
| **Custom statuses** | `StatusKind` is a fixed enum | no way to author a new effect kind |
| **Custom abilities** | `AbilityKind` is a fixed enum | no way to author a dash/area/projectile variant |
| **Damage types and resistances** | one damage number | no typed damage, no armor |
| **Projectile behaviour** | point, gravity, travel | no homing, bouncing, piercing, or AoE-on-expiry |
| **Conditional triggers** | fixed event set | no "play this while my resource is above N" |

### Far — new subsystems

| Want | Today | Gap |
| --- | --- | --- |
| **World access** | none | a package cannot read or write voxels |
| **Client-side Scheme** | server only | no custom visuals, HUD, or prediction |
| **Package dependencies** | none | no "requires base-melee v2" |
| **Distribution** | local directories | no registry, versioning, or update path |
| **Config and localization** | hardcoded strings | no settings surface, no translations |
| **Persistence** | none | no per-player or per-world package state |

## 4. What v2 should prioritise

Ordered by value to a weapon-pack author per unit of host work.

1. **On-hit effect callbacks.** The single biggest gap. A `(on-hit ctx)` hook
   that can apply a status, deal typed damage, or emit a sound turns "a sword
   with bigger numbers" into "a flame sword". It also unlocks the sound system's
   package-defined triggers, since both ride the same event.
2. **Per-weapon sound keys.** Small, and it removes the awkward "one hit sound
   for every weapon in the package".
3. **Item metadata and kinds.** `ItemKind` gains `Consumable`/`Tool`; a weapon
   gains a description and icon. This is what makes a pack feel finished.
4. **Per-weapon attack kind.** Let a package declare the kind rather than the
   host inferring it, so a beam or thrown weapon is data.
5. **Typed damage.** A damage type on the weapon and a resistance on the body.
   Needed before on-hit effects can be balanced.
6. **Projectile behaviour flags.** Piercing, bouncing, homing — each a small
   addition to the projectile spec once the effect hook exists.

Deferred deliberately: world access and client-side Scheme. Both are large,
both break the "packages are data" property that makes the current sandbox
tractable, and neither is needed for a weapon pack.

## 5. Design constraints to preserve

- **Determinism.** Packages must not be able to make the simulation
  non-replayable. Any callback runs on the server, in the tick, with no
  wall-clock or RNG the host does not own.
- **Validation at load.** Every authored value is bounded before install. New
  fields must extend that table, not bypass it.
- **Hot reload.** A candidate must be installable at a tick boundary without
  disturbing in-flight state. Effects that outlive a reload need a generation
  tag, the way arrows already carry one.
- **Client parity.** Anything the client needs to render or predict must
  replicate. A package cannot assume client-side code.

## 6. v2 direction: declarative effects, native application

The goal is more extensibility without a VM in the tick. A package **declares**
what happens; the host **applies** it natively. That keeps the tick free of
interpretation, keeps the simulation deterministic and replayable, and still
hot-reloads — the declared tables swap at a tick boundary exactly like the
weapon table does today.

### Effect primitives

An effect is a list of primitives, each a host-provided constructor:

```scheme
;; (effect . primitives)
(define flame
  (effect (damage 4 "fire")
          (status "burn" 120)
          (impulse 12.0)
          (sound "ignite")))

(define lifesteal
  (effect (heal 3)))
```

| Primitive | Meaning | Bounds |
| --- | --- | --- |
| `(damage amount type)` | typed damage to the target | amount 0–65535, type a known string |
| `(heal amount)` | restore health to the attacker | amount 0–65535 |
| `(status kind ticks)` | apply a status for a duration | kind a known string, ticks 1–36000 |
| `(impulse speed)` | knockback along the hit direction | speed 0–100 |
| `(sound event)` | play a package sound | event from the fixed set |
| `(spawn projectile)` | spawn an authored projectile | a `projectile` spec |

Every primitive is validated at load against that table, exactly like the
existing bounds. The host applies them in declaration order.

### Triggers

Effects attach to a weapon and fire on a fixed event set — the same moments the
sound system already uses, so the two share one vocabulary:

| Trigger | Fires when |
| --- | --- |
| `on-hit` | the weapon damages a character |
| `on-kill` | the weapon's damage depletes a character |
| `on-fire` | a ranged weapon releases a shot |
| `on-expire` | a projectile expires without hitting |

```scheme
(melee-weapon 9 "Flame Sword" 3.2 10 24 350.0 "flame.glb"
              (on-hit flame)
              (on-kill (effect (sound "kill"))))
```

### Why this shape

- **No VM in the tick.** The declared primitives compile into a native table;
  applying one is a match arm, not an interpretation. Bevy's frame budget is
  untouched.
- **Deterministic by construction.** There is no script to run, so there is no
  wall clock, no RNG, and no way to make the simulation non-replayable.
- **Hot reloads for free.** The table swaps at a tick boundary like every other
  package table. Edit `server.scm`, save, and the next swing uses the new
  effect — which is exactly the testing loop you want.
- **A migration path to scripting.** When a case genuinely needs logic the
  primitives cannot express, the trigger becomes a callback that *returns*
  primitives. The host API does not change; only the source of the list does.

### Client-only mods (tier A)

Cosmetic mods need no server involvement and cannot affect gameplay, so they
are safe to load locally. The client gains a local asset directory that
resolves **ahead of** the server's `pkg://` source:

```
client assets/  →  pkg:// (server packages)  →  built-in fallback
```

A local `.glb` or audio file with the same relative path overrides the server's
copy. No scripting, no VM, no determinism risk — just a resolution order. This
is the cheap 80% of "client-only mods": reskins, model swaps, sound packs.

### Deferred

- **Scripted effects** (a callback returning primitives) — the migration path
  above, once the primitives prove insufficient.
- **Client-side Scheme** — custom HUD, particles, screen effects. A second VM
  and a second API surface; do it when a concrete case demands it.
- **World access** — a package reading or writing voxels. Breaks the
  "packages are data" property; needs its own trust story.

