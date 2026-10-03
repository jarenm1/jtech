# Character controllers

`controller` owns grounded locomotion and exposes `ControllerPlugin`. `physics` owns voxel/loose-cube collision queries, validated `CollisionShape`, kinematic motion state, and impulses. Dependencies point from controller to physics to voxel_world. Protocol and applications depend on controller for human commands.

## Shared motor

`step_character` is a fixed pipeline of named stages — `status`, `sanitize`, `stance`, `contact`, `steer`, `gravity`, `sweep_horizontal`, `sweep_vertical`, `drag` — so authority, prediction and replay agree on ordering. The public contract is unchanged: intent in, `CharacterState` out.

- `CharacterIntent`: held, bounded body-relative movement and turn; a consumed one-tick jump request; a one-tick `attack` edge the host's combat system consumes (the motor ignores it); held `sprint` and `crouch` requests.
- `CharacterBody`: feet-anchored AABB dimensions and mass in kilograms.
- `MovementProfile`: speed, sprint multiplier, coyote and jump-buffer ticks, step height, walkable-slope cosine, crouch multiplier and height, acceleration, braking, air control, strafe fraction, yaw rate, gravity, jump speed, and external-momentum drag.
- `CharacterState`: motion, facing, locomotion `mode`, active `statuses`, coyote/buffer ticks and crouch flag — the complete replay unit. Horizontal external momentum is separate from controlled movement.

At yaw zero, forward is world -Z and right is +X. Positive yaw/turn rotates left. Movement axes and turn are clamped to [-1, 1]; diagonal movement is limited to unit magnitude. Turn is multiplied by the profile's radians/second. Non-finite actions become zero. Shape/mass constructors reject invalid configuration; profile values are bounded at the motor boundary. Ticks must be finite and positive, with catch-up capped at 0.25 seconds.

Call `step_character` directly, or spawn the four components together and use the plugin. `movement` and `turn` persist until replaced. `jump` is consumed during a valid tick even when airborne or jumping is disabled, then buffered for `jump_buffer_ticks` and honoured inside the `coyote_ticks` window after leaving the ground. A policy deciding every N ticks should submit jump once and retain held actions across those ticks. Include actions, initial state, profiles, timesteps, terrain and collider snapshots when reproducing an episode. Exact replay tests cover the same executable and inputs, not cross-platform floating-point determinism.

Ground locomotion includes sliding, support detection, jumping, loose-cube contacts, and automatic step-up: a blocked horizontal sweep retries from a pose lifted by `step_height`, so low obstructions are climbed while a ceiling still blocks. Terrain collision is a smooth density sweep: the leading face samples trilinear `VoxelWorld::density_at`, so actors rest on the iso surface. The support probe reports the ground normal; ground steeper than `max_slope_cos` overrides control and slides the actor downhill. Crouch lowers the effective collision shape to `crouch_height` and scales speed by `crouch_mult`; standing up requires clearance for the full-height body. Placed cubes and unloaded chunks stay discrete solids.

## Status effects

`CharacterState.statuses` is a bounded list of `(kind, remaining_ticks)` entries. `StatusList::apply` is strongest-wins per kind — a longer remaining duration replaces a shorter one — and a full list drops the new status rather than evicting an active one. Each tick decrements and expires entries, then derives `Constraints`: stun, sleep and knockup lock action and movement; root locks movement; silence locks casts; slow scales speed. Knockback stays physics-owned as an impulse, not a status. Taunt, fear and blind are host-level (AI and vision), not motor gates.

## Bevy hosts and headless execution

`ControllerPlugin::default()` runs in `FixedUpdate`. Configure the host's fixed clock to **60 Hz** explicitly: Bevy's default fixed timestep is different. With full Bevy use `Time::<Fixed>::from_hz(60.0)`. In a minimal/headless `bevy_app::App`, call `world_mut().run_schedule(FixedUpdate)` once per simulated 1/60 second. Alternatively, use `ControllerPlugin::on(schedule)` with a host schedule that already runs at 60 Hz.

Insert the authoritative **`VoxelWorld` resource** into the same Bevy world. The plugin initializes a default resource only if none exists; it does not maintain a separate terrain copy. Unloaded terrain is solid. Populate **`ObservedBodies`** with current loose-cube snapshots before the motor, using a system in `ControllerSet::Intent`. Snapshots are not extrapolated. Order an intent producer after a sensor/snapshot system explicitly when it needs that system's outputs. The motor runs in `ControllerSet::Motor`, after intent production.

The server installs the plugin on its manually paced `Update` schedule, after the authoritative simulation step. Controller actors use that simulation's `VoxelWorld`; each tick the host copies current physics colliders into `ObservedBodies`. The client installs the default plugin with a 60 Hz fixed clock and copies its received loose-cube observations. Existing human players are advanced explicitly by the shared adapter, not also spawned as ECS motor entities.

For a minimal scripted NPC, spawn `(CharacterState, CharacterBody, MovementProfile, CharacterIntent)` and update its intent in `ControllerSet::Intent`. The headless plugin test exercises this path with an animal body/profile and compares it to direct motor stepping. NPC replication, terrain interest management, combat, and animation are separate host responsibilities. Current controller actors collide with terrain and observed loose cubes; actor-to-actor collision and NPC force feedback into the GPU solver are not wired yet.

## Human adapter

`controller::player` translates `PlayerInput` into the shared motor contract. Authority, prediction, reconciliation, and the smoke client all use this adapter. Packet sequencing and deduplication are handled by the existing networking/simulation code. Human commands carry absolute view yaw, so the adapter initializes facing from that yaw; generic policies use body-relative turn instead. The existing wire layout is unchanged.

Ground jumping uses a buffered key-press edge: capture it on a rendered frame, consume it in one simulation command, and keep it pending when no simulation tick runs. Debug noclip uses held ascent/descent and is implemented separately in the human adapter. It is not exposed as a learned-policy action. The underlying `physics::PlayerState` compatibility name aliases `KinematicState` for existing snapshots and collision consumers.

## Vision-based experiments

Observation construction is independent of motor actions:

`agent-camera pixels -> policy -> CharacterIntent -> motor -> world`

A vision adapter can render the agent camera at a known simulation tick, run inference, then apply bounded movement/turn plus one-shot requests for a specified number of ticks. Explicitly choose whether to expose proprioception. Do not pass `CharacterState`, world coordinates, collider snapshots, or privileged terrain data to a pixels-only policy; these are motor inputs, not automatically policy observations. Keep reward calculation and seeded reset outside the motor.

Camera rendering, training infrastructure, swimming/flight, and articulated joint control are later adapters or motor implementations. The current motor provides game-level walking controls rather than learned gait or torque control.
