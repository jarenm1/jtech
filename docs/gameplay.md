# Gameplay foundation

`gameplay::Health` is a shared Bevy component with whole-point current and maximum
health. Players start at 100/100. Damage saturates at zero; healing saturates at
the maximum. Both operations return the actual change. Healing from zero is
allowed, and `restore` fills health. A custom maximum must be positive.

The authoritative server stores health separately from movement. Gameplay code
can read `Simulation::player_health` and use `damage_player`, `heal_player`, or
`restore_player_health`. Unknown or disconnected player IDs return `None`.
Clients receive health in the welcome message and the 20 Hz player snapshots,
and display it in the HUD. Health is not predicted during movement replay.
Protocol version 9 requires matching client and server builds.

This slice provides health and depletion state. Death/respawn rules and damage
sources are separate gameplay work. For moving-block damage, convert a confirmed
server-side impact to health points and call `damage_player` once for that event.
Use stable player/body IDs across asynchronous physics readback, and distinguish
new impacts from resting contacts before applying damage.

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
boost. Press **R** to cycle power; higher presets give stronger, wider blasts.
At standard power, a close ground shot can lift you several blocks.
Press **V** to leave noclip before trying a blast jump.

Nearby players, including the shooter, receive server-authoritative impulses.
Terrain and loose blocks shield players; distance and partial exposure reduce
force. Horizontal momentum carries through movement and decays with drag,
while gravity and collisions govern the jump. Noclip players ignore blasts.
Player knockback works with or without GPU physics.

## Game interface

Select a hotbar slot with **1–6**. Read health at the lower left and the selected
item above the centered hotbar. Package status and reload errors appear at the
upper right; the FPS counter is at the upper left.

Press **Esc** to open the translucent pause menu. Choose **Resume** or press
**Esc** again to return, adjust **Bow power**, or choose **Quit game** to exit.
The menu blocks local movement, aiming and weapon input; multiplayer simulation
and network updates continue. Switching window focus opens the menu too.
