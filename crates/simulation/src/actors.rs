//! Server-owned non-player characters driven by `CharacterIntent`.
//!
//! Actors share the character motor with human players and future policies: the
//! training dummy is the degenerate policy with an empty intent. Melee swings
//! arrive through the same input/intent channel a learned agent would use, and
//! `resolve_attacks` runs inside the deterministic simulation tick.
use super::{Player, Simulation};
use controller::{
    CharacterBody, CharacterIntent, CharacterState, MovementProfile, step_character,
};
use gameplay::{
    Health,
    combat::{SwingTarget, resolve_swing},
};
use glam::{Vec2, Vec3};
use physics::{
    CollisionShape, DynamicCollider, EYE_HEIGHT, FIXED_DT, PlayerState, apply_player_impulse,
    look_direction,
};
use protocol::ActorSnapshot;
use std::collections::HashMap;
use voxel_world::VoxelWorld;

/// Fixed ticks a dead actor waits before respawning at its spawn point.
const ACTOR_RESPAWN_TICKS: u64 = 300;
/// High bit separates actor ids from player ids inside swing target lists.
const ACTOR_TARGET: u64 = 1 << 63;
/// Bound on the replicated actor set.
pub const MAX_ACTORS: usize = 32;

/// A server character with no network session: motor state, tuning, health,
/// and the intent a policy writes each tick.
pub(super) struct Actor {
    pub state: CharacterState,
    pub body: CharacterBody,
    pub profile: MovementProfile,
    pub intent: CharacterIntent,
    pub health: Health,
    pub spawn: Vec3,
    pub respawn_at: Option<u64>,
    pub attack_ready: u64,
}

impl Actor {
    fn snapshot(&self, id: u32) -> ActorSnapshot {
        ActorSnapshot {
            id,
            state: self.state.motion,
            health: self.health,
            yaw: self.state.yaw,
        }
    }
}

impl Simulation {
    /// Spawn a static training actor at a supported position. Returns its id.
    pub fn spawn_actor(&mut self, position: Vec3) -> Option<u32> {
        if self.actors.len() >= MAX_ACTORS || !position.is_finite() {
            return None;
        }
        let id = self.next_actor;
        self.next_actor = self.next_actor.wrapping_add(1).max(1);
        self.actors.insert(
            id,
            Actor {
                state: CharacterState {
                    motion: PlayerState {
                        position,
                        ..Default::default()
                    },
                    yaw: 0.0,
                },
                body: CharacterBody::default(),
                profile: MovementProfile::default(),
                intent: CharacterIntent::default(),
                health: Health::default(),
                spawn: position,
                respawn_at: None,
                attack_ready: 0,
            },
        );
        Some(id)
    }

    /// Step every living actor one fixed tick and revive actors whose respawn
    /// delay elapsed. Held movement persists; `resolve_attacks` consumes the
    /// attack edge after the motor step.
    pub(super) fn advance_actors(&mut self, world: &VoxelWorld, bodies: &[DynamicCollider]) {
        for actor in self.actors.values_mut() {
            if actor.health.is_depleted() {
                if self.tick >= actor.respawn_at.unwrap_or(u64::MAX) {
                    actor.state.motion.position = actor.spawn;
                    actor.state.motion.velocity = Vec3::ZERO;
                    actor.state.motion.external_velocity = Vec2::ZERO;
                    actor.health.restore();
                    actor.respawn_at = None;
                }
                continue;
            }
            let mut intent = actor.intent;
            step_character(
                world,
                &mut actor.state,
                &actor.body,
                &actor.profile,
                &mut intent,
                FIXED_DT,
                bodies,
            );
            actor.intent = intent;
        }
    }

    /// Resolve one melee swing per attacker per tick: players swing along their
    /// input yaw/pitch, actors along their state yaw with level aim. Cooldowns
    /// apply to misses and hits alike; dead players and noclip attackers cannot
    /// swing.
    pub(super) fn resolve_attacks(&mut self, world: &VoxelWorld) {
        let attackers: Vec<u64> = self
            .players
            .iter()
            .filter(|(_, player)| {
                !player.health.is_depleted()
                    && !player.state.noclip
                    && player.input.attack
                    && self.tick >= player.attack_ready
            })
            .map(|(&id, _)| id)
            .collect();
        // Actor intents carry the same attack edge; consume it here so a policy
        // sees one swing per written edge, matching the player input channel.
        let actor_attackers: Vec<u32> = self
            .actors
            .iter()
            .filter(|(_, actor)| {
                !actor.health.is_depleted()
                    && actor.intent.attack
                    && self.tick >= actor.attack_ready
            })
            .map(|(&id, _)| id)
            .collect();
        for actor_id in actor_attackers {
            self.swing_actor(world, actor_id);
        }
        for id in attackers {
            let spec = gameplay::combat::melee_spec(gameplay::combat::slot_item(
                self.players[&id].input.selected,
            ));
            let player = &self.players[&id];
            let origin = player.state.position + Vec3::Y * EYE_HEIGHT;
            let direction = look_direction(player.input.yaw, player.input.pitch);
            let mut targets: Vec<SwingTarget> = self
                .actors
                .iter()
                .filter(|(_, actor)| !actor.health.is_depleted())
                .map(|(&actor_id, actor)| SwingTarget {
                    id: u64::from(actor_id) | ACTOR_TARGET,
                    position: actor.state.motion.position,
                    shape: actor.body.shape,
                })
                .collect();
            targets.extend(
                self.players
                    .iter()
                    .filter(|(other, player)| {
                        **other != id && !player.health.is_depleted() && !player.state.noclip
                    })
                    .map(|(&other, player)| SwingTarget {
                        id: other,
                        position: player.state.position,
                        shape: CollisionShape::default(),
                    }),
            );
            self.players.get_mut(&id).unwrap().attack_ready =
                self.tick + u64::from(spec.cooldown_ticks);
            let Some(hit) = resolve_swing(world, origin, direction, &spec, &targets) else {
                continue;
            };
            if hit.target & ACTOR_TARGET != 0 {
                let actor_id = (hit.target & !ACTOR_TARGET) as u32;
                if let Some(actor) = self.actors.get_mut(&actor_id) {
                    actor.state.apply_impulse(&actor.body, hit.impulse);
                    actor.health.damage(hit.damage);
                    if actor.health.is_depleted() {
                        actor.respawn_at = Some(self.tick + ACTOR_RESPAWN_TICKS);
                    }
                }
            } else {
                // Impulse first: a killing blow zeroes momentum through damage_player.
                if let Some(victim) = self.players.get_mut(&hit.target) {
                    apply_player_impulse(&mut victim.state, hit.impulse);
                }
                self.damage_player(hit.target, hit.damage);
            }
        }
    }

    /// One actor swing: level aim along the actor's yaw against every other
    /// living actor and player. Consumes the intent edge and starts cooldown.
    fn swing_actor(&mut self, world: &VoxelWorld, id: u32) {
        let spec = gameplay::combat::melee_spec(self.actors[&id].intent.held_item);
        let actor = &self.actors[&id];
        let origin = actor.state.motion.position + Vec3::Y * EYE_HEIGHT;
        let direction = look_direction(actor.state.yaw, 0.0);
        let mut targets: Vec<SwingTarget> = self
            .actors
            .iter()
            .filter(|(other, actor)| **other != id && !actor.health.is_depleted())
            .map(|(&other, actor)| SwingTarget {
                id: u64::from(other) | ACTOR_TARGET,
                position: actor.state.motion.position,
                shape: actor.body.shape,
            })
            .collect();
        targets.extend(
            self.players
                .iter()
                .filter(|(_, player)| !player.health.is_depleted() && !player.state.noclip)
                .map(|(&other, player)| SwingTarget {
                    id: other,
                    position: player.state.position,
                    shape: CollisionShape::default(),
                }),
        );
        let actor = self.actors.get_mut(&id).unwrap();
        actor.intent.attack = false;
        actor.attack_ready = self.tick + u64::from(spec.cooldown_ticks);
        let Some(hit) = resolve_swing(world, origin, direction, &spec, &targets) else {
            return;
        };
        if hit.target & ACTOR_TARGET != 0 {
            let target_id = (hit.target & !ACTOR_TARGET) as u32;
            if let Some(victim) = self.actors.get_mut(&target_id) {
                victim.state.apply_impulse(&victim.body, hit.impulse);
                victim.health.damage(hit.damage);
                if victim.health.is_depleted() {
                    victim.respawn_at = Some(self.tick + ACTOR_RESPAWN_TICKS);
                }
            }
        } else {
            if let Some(victim) = self.players.get_mut(&hit.target) {
                apply_player_impulse(&mut victim.state, hit.impulse);
            }
            self.damage_player(hit.target, hit.damage);
        }
    }

    /// Actor snapshots visible to a player, filtered by chunk interest like
    /// remote players so distant actors cost nothing.
    pub(super) fn actor_snapshots(&self, viewer: &Player) -> Vec<ActorSnapshot> {
        self.actors
            .iter()
            .filter(|(_, actor)| {
                viewer.interest.contains(&voxel_world::chunk_coord(
                    actor.state.motion.position.floor().as_ivec3(),
                ))
            })
            .map(|(&id, actor)| actor.snapshot(id))
            .collect()
    }
}

/// Read-only view of the actor set for tests and smoke tooling.
impl Simulation {
    pub fn actor_health(&self, id: u32) -> Option<Health> {
        self.actors.get(&id).map(|actor| actor.health)
    }
    pub fn actor_position(&self, id: u32) -> Option<Vec3> {
        self.actors.get(&id).map(|actor| actor.state.motion.position)
    }
    pub fn actor_ids(&self) -> Vec<u32> {
        self.actors.keys().copied().collect()
    }
}

/// Re-exported so `Simulation` can store the map without leaking the type.
pub(super) type ActorMap = HashMap<u32, Actor>;
