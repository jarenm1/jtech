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
Protocol version 6 requires matching client and server builds.

This slice provides health and depletion state. Death/respawn rules and damage
sources are separate gameplay work. For moving-block damage, convert a confirmed
server-side impact to health points and call `damage_player` once for that event.
Use stable player/body IDs across asynchronous physics readback, and distinguish
new impacts from resting contacts before applying damage.

Run `cargo test -p gameplay -p protocol -p simulation -p voxel-client` for the
component, wire validation, server integration, and client checks.
