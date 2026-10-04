# Gameplay foundation

`gameplay::Health` is a shared Bevy component with whole-point current and maximum
health. Players start at 100/100. Damage saturates at zero; healing saturates at
the maximum. Both operations return the actual change. The component allows
healing from zero; the server's player lifecycle requires respawn instead.
A custom maximum must be positive.

The authoritative server stores health separately from movement. Gameplay code
can read `Simulation::player_health` and use `damage_player`, `heal_player`, or
`restore_player_health`. Unknown or disconnected player IDs return `None`.
Clients receive health in the welcome message and the 20 Hz player snapshots,
and display it in the HUD. Health is not predicted during movement replay.
Protocol version 18 requires matching client and server builds.

## Blast damage and respawn

Blasts damage each exposed player once, including the shooter. Damage is rounded
to whole points: `60 × exposure × (1 − distance / radius)²`. Distance is measured
to the player's center; exposure is the fraction of three clear rays at quarter,
half and three-quarter height. Terrain and loose-block shielding are sampled
before the explosion changes the world. Noclip players ignore blasts.

One blast cannot kill a full-health player, so ordinary blast jumps are
survivable. Larger bow presets extend the dangerous area. Repeated close shots
can kill. At zero HP, movement, firing and terrain edits are blocked by the
server; queued input, momentum and pending strikes are cleared.

Press **Enter** or click the respawn button on the death overlay to return with full
health and zero momentum in walking mode. The server searches loaded, supported,
collision-free space near the original spawn, using the current edited terrain.
The request waits for GPU readback and, if needed, asynchronous loading of a
small fallback area. You stay on the death overlay until safe support is available.
Each successful respawn advances a life counter: old movement packets and
repeated respawn requests cannot affect the new life, and the client discards
prediction from the previous life.

For moving-block damage, convert a confirmed server-side impact to health points
and call `damage_player` once for that event. Use stable player/body IDs across
asynchronous physics readback, and distinguish new impacts from resting contacts.

Run `cargo test -p gameplay -p protocol -p simulation -p voxel-client` for the
component, wire validation, server integration, and client checks.

## Noclip flight

Press **V** with the mouse captured to toggle flight. Use **WASD** relative to
where you look, **Space** to rise, and **Ctrl** to descend. Flight is 12 m/s,
without gravity or terrain/loose-block collision; release movement to hover.
The server applies the same movement as client prediction and replicates the mode.
Press **V** again to walk. If inside a block, fly clear first; walking resumes
once the whole player fits in loaded empty space.

## Blast jumping

Equip the bow with **6**, aim steeply down at the ground near your feet, and fire
with **right click** to launch yourself. Shoot beside or behind you for a sideways
boost. The draw takes a second and fires at full power; a close ground shot can
lift you several blocks.
Press **V** to leave noclip before trying a blast jump.

Nearby players, including the shooter, receive server-authoritative impulses.
Terrain and loose blocks shield players; distance and partial exposure reduce
force. Horizontal momentum carries through movement and decays with drag,
while gravity and collisions govern the jump. Noclip players ignore blasts.
Player knockback works with or without GPU physics.

## Items and drops

Destroying a block leaves a dropped stack carrying the material's item id. A drop
pops out, falls under gravity, and settles on the first solid surface below it;
if that support is later removed it resumes falling. Drops despawn after five
minutes, and the replicated set is bounded at `protocol::MAX_DROPS` (64), evicting
the oldest.

`gameplay::Inventory` keeps one count per placeable material (ids 1 through 5,
matching the block materials) with a per-kind ceiling of `gameplay::MAX_STACK`
(999). The server owns every mutation: each authoritative destruction path calls
`spawn_drop`, and `collect_drops` merges stacks into living players within 1.8 m,
lowest player id first, after a half-second arming delay. A full stack leaves the
remainder in the world.

Counts reach clients reliably on change (`ServerMessage::Inventory`, also carried
in the welcome) and the complete drop set is replaced on change, up to 20 Hz
(`ServerMessage::Drops`). The client renders drops as bobbing cubes and shows
owned counts on the hotbar tiles; it holds no authoritative drop or item state.
Item ids currently match block materials; richer item behavior can be added
through a future registry.

## Melee combat

Left click is the general attack action: with a weapon held it attacks — a melee swing or a ranged shot — regardless of what is under the crosshair; with a block or an empty hand it mines terrain. A melee weapon swings once per press; a ranged weapon draws while held and fires on release at full charge. Swings
ride the input
stream (`PlayerInput.attack`), so the same channel serves scripted policies. The server resolves one swing
per tick per player through `gameplay::combat::resolve_swing`: a ray from the
eye must reach a feet-anchored AABB before terrain blocks it. Hits apply
`MeleeSpec` damage and a directional knockback impulse; a killing blow still
launches the victim before death zeroes momentum.

A training dummy spawns near the player spawn. It is a server `Actor` — the
shared character motor with an empty `CharacterIntent` — so it walks, takes
knockback, and collides with terrain exactly like a player. At zero health it
disappears and respawns at its spawn point after five seconds. Actor state
replicates in the 20 Hz snapshot (`ActorSnapshot`) filtered by chunk interest;
clients render a capsule that flashes on damage.
`MeleeSpec` (range, damage, cooldown ticks, knockback) is the extension point
for authored weapons; `MELEE_HANDS` is the built-in unarmed default. The
`melee` server package registers weapons into a shared item table — a Sword (7,
melee) and a Bow (8, ranged) — and declares a spawn loadout granted on connect
and respawn. A `ranged-weapon` also authors its projectile's speed, gravity,
travel and lifetime; its basic attack fires that projectile instead of a swing.
A plain projectile damages the character it strikes directly and expires on
terrain; a projectile that authors a blast detonates on any impact. Projectiles
are not expired by leaving loaded terrain, so a shot straight up falls back.
Multiple melee packages merge into the same table; conflicting item
ids error the later package. Weapons are equipment: each copy occupies its own
inventory slot (duplicates collect normally — future per-item meta needs the
slot), they drop singly on death, and their tiles show no stack count.
Registered weapons require ownership: selecting an unowned weapon refuses the
swing, while blocks, the bow, and
empty hands swing the unarmed default. Specs, names, kind and ranged flag
replicate in the package message; enemy policies write `CharacterIntent.attack`
the same way human input maps to it.

Swings land in hit zones: the top quarter of the target's height is the head
zone and takes double damage.

Death is full-loot: the transition to zero health scatters the carried
inventory as drops around the body and empties the corpse. Drops behave like
mined items — a short pickup delay, then any living player within reach
collects them. Respawning restores health only; gear must be recovered or
replaced.

## Game interface

Select a hotbar slot with **1–0**. Read health at the lower left, the selected
item above the centered hotbar, and owned item counts on the hotbar tiles.
Package status and reload errors appear at the upper right; the FPS counter is at
the upper left.

**Tab** opens the inventory panel: a scrollable grid of every owned stack plus
the explosive bow. Drag a cell onto a hotbar slot to assign it, drag between
hotbar slots to swap, and drop a hotbar item back on the grid to clear the
slot. The panel releases the cursor and blocks gameplay input; **Esc** closes
it before the pause menu. Inventories are unbounded — new item kinds append to
the grid and stack counts are uncapped.

Press **Esc** to open the translucent pause menu. Choose **Resume** or press
**Esc** again to return, or choose **Quit game** to exit.
The menu blocks local movement, aiming and weapon input; multiplayer simulation
and network updates continue. Switching window focus opens the menu too.
