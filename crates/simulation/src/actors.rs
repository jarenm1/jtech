//! Server-owned non-player characters driven by `CharacterIntent`.
//!
//! Actors share the character motor with human players and future policies.
//! Each actor carries a species (`ActorKind`) plus a brain: `External` actors
//! are driven through `set_actor_intent`, `Scripted` actors run a crate
//! `brains::Brain` against the same `ActorObservation` a learned policy sees.
//! Melee swings arrive through the same input/intent channel a learned agent
//! would use, and `resolve_attacks` runs inside the deterministic simulation
//! tick.
use super::{Player, Simulation};
use controller::{
    Cast, CharacterBody, CharacterIntent, CharacterState, MAX_SLOTS, Mode, MovementProfile,
    StatusList, step_character,
};
use gameplay::{
    Health,
    combat::{MeleeSpec, SwingTarget, resolve_swing},
};
use glam::{IVec3, Vec2, Vec3};
use physics::{
    CollisionShape, DynamicCollider, EYE_HEIGHT, FIXED_DT, PlayerState, apply_player_impulse,
    look_direction,
};
use protocol::ActorSnapshot;
use std::collections::{BTreeMap, HashMap, VecDeque};
use voxel_world::VoxelWorld;

/// Fixed ticks a dead actor waits before respawning at its spawn point.
const ACTOR_RESPAWN_TICKS: u64 = 300;
/// High bit separates actor ids from player ids inside swing target lists.
const ACTOR_TARGET: u64 = 1 << 63;
/// Bound on the replicated actor set.
pub const MAX_ACTORS: usize = 32;

/// Actions a species is allowed to express, bit flags over intent fields.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActorCapabilities(pub u32);
impl ActorCapabilities {
    pub const JUMP: Self = Self(1 << 0);
    pub const MELEE: Self = Self(1 << 1);
    /// Held-item resolution through the melee table; innate `melee` otherwise.
    pub const ITEM: Self = Self(1 << 2);
    pub const NONE: Self = Self(0);
    pub const fn all() -> Self {
        Self(u32::MAX)
    }
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }
}

/// Which policy drives an actor. `External` actors never run a scripted brain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrainKind {
    External,
    Idle,
    Hunter,
    Flee,
}

/// Species data: one table entry per spawnable actor archetype.
#[derive(Clone, Copy, Debug)]
pub struct ActorKind {
    pub name: &'static str,
    pub body: CharacterBody,
    pub profile: MovementProfile,
    /// Maximum health; `Health::new(health)` at spawn.
    pub health: u16,
    pub capabilities: ActorCapabilities,
    /// Innate weapon; `ITEM` actors may resolve equipment later.
    pub melee: MeleeSpec,
    pub brain: BrainKind,
}
impl ActorKind {
    /// Today's training dummy: player defaults, hands, stands still.
    pub fn dummy() -> Self {
        Self {
            name: "dummy",
            body: CharacterBody::default(),
            profile: MovementProfile::default(),
            health: gameplay::PLAYER_MAX_HEALTH,
            capabilities: ActorCapabilities::all(),
            melee: gameplay::combat::MELEE_HANDS,
            brain: BrainKind::Idle,
        }
    }
    /// Heavy melee bruiser: slow, thick, hunts the nearest player.
    pub fn titan() -> Self {
        Self {
            name: "titan",
            body: CharacterBody::new(
                CollisionShape::new(0.8, 0.8, 2.6).unwrap_or_default(),
                600.0,
            )
            .unwrap_or_default(),
            profile: MovementProfile {
                speed: 3.0,
                ..MovementProfile::default()
            },
            health: 400,
            capabilities: ActorCapabilities::MELEE,
            melee: MeleeSpec {
                range: 6.0,
                damage: 40,
                cooldown_ticks: 60,
                knockback: 800.0,
            },
            brain: BrainKind::Hunter,
        }
    }
    /// Fragile training prey: unarmed, flees attackers.
    pub fn decoy() -> Self {
        Self {
            name: "decoy",
            body: CharacterBody::new(
                CollisionShape::new(0.25, 0.25, 1.2).unwrap_or_default(),
                40.0,
            )
            .unwrap_or_default(),
            profile: MovementProfile {
                speed: 4.0,
                ..MovementProfile::default()
            },
            health: 100,
            capabilities: ActorCapabilities::JUMP,
            melee: MeleeSpec {
                range: 0.5,
                damage: 1,
                cooldown_ticks: 60,
                knockback: 0.0,
            },
            brain: BrainKind::Flee,
        }
    }
}

/// Entity reference inside simulation events and observations.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SimEntity {
    Player(u64),
    Actor(u32),
}

/// Per-tick attribution event pushed onto `Simulation::events`. The backlog is
/// bounded at `MAX_QUEUED_EVENTS`; when full, the oldest events drop first.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SimEvent {
    DamageDealt {
        source: SimEntity,
        target: SimEntity,
        /// Points actually lost, capped at the victim's remaining health.
        amount: u16,
        killed: bool,
    },
    ActorDied {
        id: u32,
        killer: Option<SimEntity>,
    },
    /// A player reached zero health; `killer` is who landed the final hit.
    PlayerDied {
        id: u64,
        killer: Option<SimEntity>,
    },
}

/// Backlog bound for undrained `SimEvent`s so a consumer that never calls
/// `drain_events` cannot grow memory; the most recent events are kept.
pub(super) const MAX_QUEUED_EVENTS: usize = 4096;

/// One nearby entity as a policy sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObservedEntity {
    pub entity: SimEntity,
    pub position: Vec3,
    pub velocity: Vec3,
    pub distance: f32,
    /// False when terrain occludes the eye-to-eye segment.
    pub line_of_sight: bool,
}

/// The flat observation a brain consumes each tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActorObservation {
    pub tick: u64,
    pub position: Vec3,
    pub velocity: Vec3,
    pub yaw: f32,
    pub grounded: bool,
    pub health: Health,
    /// Own melee cooldown elapsed.
    pub attack_ready: bool,
    /// Own innate melee range, for spacing decisions.
    pub melee_range: f32,
    pub nearest_player: Option<ObservedEntity>,
    pub nearest_actor: Option<ObservedEntity>,
    /// Own locomotion/action mode.
    pub mode: Mode,
    /// Own active status effects.
    pub statuses: StatusList,
    /// Own remaining cooldown ticks per ability slot.
    pub cooldowns: [u16; MAX_SLOTS],
    /// Own in-progress cast, if any.
    pub cast: Option<Cast>,
    /// Whether the body is crouched.
    pub crouching: bool,
}

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
    pub kind: ActorKind,
    pub brain: ActorBrain,
}

/// Runtime policy holder. `External` reads the stored intent; `Scripted`
/// derives one per tick inside `advance_actors`.
pub(super) enum ActorBrain {
    External,
    Scripted(Box<dyn crate::brains::Brain>),
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

/// Bound the intent and zero fields the species cannot express.
fn gate_intent(intent: CharacterIntent, capabilities: ActorCapabilities) -> CharacterIntent {
    let mut intent = intent.bounded();
    if !capabilities.contains(ActorCapabilities::JUMP) {
        intent.jump = false;
    }
    if !capabilities.contains(ActorCapabilities::MELEE) {
        intent.attack = false;
    }
    if !capabilities.contains(ActorCapabilities::ITEM) {
        intent.held_item = 0;
    }
    intent
}

/// Eye-to-eye terrain occlusion; the raycast normalizes the direction.
fn line_of_sight(world: &VoxelWorld, from: Vec3, to: Vec3) -> bool {
    let eye = from + Vec3::Y * EYE_HEIGHT;
    let target_eye = to + Vec3::Y * EYE_HEIGHT;
    let offset = target_eye - eye;
    world
        .raycast(eye, offset, offset.length())
        .is_none()
}

/// Build one actor's policy view: own state plus the nearest living player
/// and actor. `self_id` excludes the observer from its own actor search.
/// Depleted actors still observe (respawn bookkeeping needs nothing here, but
/// `observe` answers for any existing id).
fn observation_of(
    actor: &Actor,
    self_id: u64,
    actors: &ActorMap,
    players: &HashMap<u64, Player>,
    world: &VoxelWorld,
    tick: u64,
) -> ActorObservation {
    let feet = actor.state.motion.position;
    let observe = |entity: SimEntity, position: Vec3, velocity: Vec3| ObservedEntity {
        entity,
        position,
        velocity,
        distance: position.distance(feet),
        line_of_sight: line_of_sight(world, feet, position),
    };
    // Candidates rank on distance alone, so only the winner pays for the
    // `observe` terrain raycast; ties resolve like the old `min_by`.
    let nearest_player = players
        .iter()
        .filter(|(_, player)| !player.health.is_depleted() && !player.state.motion.noclip)
        .min_by(|(_, a), (_, b)| {
            a.state.motion
                .position
                .distance_squared(feet)
                .total_cmp(&b.state.motion.position.distance_squared(feet))
        })
        .map(|(&id, player)| {
            observe(
                SimEntity::Player(id),
                player.state.motion.position,
                player.state.motion.velocity,
            )
        });
    let nearest_actor = actors
        .iter()
        .filter(|&(&id, other)| u64::from(id) != self_id && !other.health.is_depleted())
        .min_by(|(_, a), (_, b)| {
            a.state
                .motion
                .position
                .distance_squared(feet)
                .total_cmp(&b.state.motion.position.distance_squared(feet))
        })
        .map(|(&id, other)| {
            observe(
                SimEntity::Actor(id),
                other.state.motion.position,
                other.state.motion.velocity,
            )
        });
    ActorObservation {
        tick,
        position: feet,
        velocity: actor.state.motion.velocity,
        yaw: actor.state.yaw,
        grounded: actor.state.motion.grounded,
        health: actor.health,
        attack_ready: tick >= actor.attack_ready,
        melee_range: actor.kind.melee.range,
        nearest_player,
        nearest_actor,
        mode: actor.state.mode,
        statuses: actor.state.statuses,
        cooldowns: actor.state.cooldowns,
        cast: actor.state.cast,
        crouching: actor.state.crouching,
    }
}

impl Simulation {
    /// Spawn a static training actor at a supported position. Returns its id.
    pub fn spawn_actor(&mut self, position: Vec3) -> Option<u32> {
        self.spawn_actor_kind(ActorKind::dummy(), position)
    }

    /// Spawn one instance of a species at a supported position. Returns its id.
    pub fn spawn_actor_kind(&mut self, kind: ActorKind, position: Vec3) -> Option<u32> {
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
                    ..Default::default()
                },
                body: kind.body,
                profile: kind.profile,
                intent: CharacterIntent::default(),
                health: Health::new(kind.health).unwrap_or_default(),
                spawn: position,
                respawn_at: None,
                attack_ready: 0,
                kind,
                brain: match kind.brain {
                    BrainKind::External => ActorBrain::External,
                    scripted => ActorBrain::Scripted(crate::brains::build_brain(scripted)),
                },
            },
        );
        Some(id)
    }

    /// Drive an actor externally; the intended path for `BrainKind::External`
    /// actors but accepted for any kind (scripted brains overwrite it next
    /// tick). Returns false for an absent id.
    pub fn set_actor_intent(&mut self, id: u32, intent: CharacterIntent) -> bool {
        let Some(actor) = self.actors.get_mut(&id) else {
            return false;
        };
        actor.intent = gate_intent(intent, actor.kind.capabilities);
        true
    }

    /// Push one event onto the bounded backlog, dropping the oldest when full.
    pub(super) fn push_event(&mut self, event: SimEvent) {
        push_event(&mut self.events, event);
    }

    /// Apply player damage with event attribution: `DamageDealt` for every
    /// landed hit, plus `PlayerDied` when this hit depleted the victim.
    /// Returns the points lost, like `damage_player`.
    pub(super) fn damage_player_event(
        &mut self,
        source: SimEntity,
        target: u64,
        damage: u16,
    ) -> Option<u16> {
        let amount = self.damage_player(target, damage)?;
        // A hit on an already-dead player loses nothing and is not a new kill.
        let killed = amount > 0 && self.players[&target].health.is_depleted();
        self.push_event(SimEvent::DamageDealt {
            source,
            target: SimEntity::Player(target),
            amount,
            killed,
        });
        if killed {
            self.push_event(SimEvent::PlayerDied {
                id: target,
                killer: Some(source),
            });
        }
        Some(amount)
    }

    /// One actor's policy view for tests, trainers and inspection tools.
    pub fn observe(&self, id: u32, world: &VoxelWorld) -> Option<ActorObservation> {
        let actor = self.actors.get(&id)?;
        Some(observation_of(
            actor,
            u64::from(id),
            &self.actors,
            &self.players,
            world,
            self.tick,
        ))
    }

    /// Step every living actor one fixed tick and revive actors whose respawn
    /// delay elapsed. Held movement persists; `resolve_attacks` consumes the
    /// attack edge after the motor step.
    pub(super) fn advance_actors(
        &mut self,
        world: &VoxelWorld,
        bodies: &[DynamicCollider],
        characters: &[DynamicCollider],
    ) {
        // Observations see pre-step state for every living actor at once so
        // brains never observe a half-advanced tick. The map leaves self.actors
        // while observations borrow it.
        let actors = std::mem::take(&mut self.actors);
        let players = &self.players;
        let observations = &mut self.actor_observations;
        observations.clear();
        observations.extend(actors.iter().filter_map(|(&id, actor)| {
            if actor.health.is_depleted() {
                return None;
            }
            Some((
                id,
                observation_of(actor, u64::from(id), &actors, players, world, self.tick),
            ))
        }));
        self.actors = actors;
        for (&id, actor) in self.actors.iter_mut() {
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
            let mut intent = match &mut actor.brain {
                ActorBrain::Scripted(brain) => brain.act(&observations[&id], self.tick),
                ActorBrain::External => actor.intent,
            };
            intent = gate_intent(intent, actor.kind.capabilities);
            // Loose bodies plus every other character, so actors collide.
            let mut step_bodies = std::mem::take(&mut self.step_scratch);
            step_bodies.clear();
            step_bodies.extend_from_slice(bodies);
            step_bodies.extend(
                characters
                    .iter()
                    .filter(|c| c.id != (super::CHARACTER_ID_BASE + u64::from(id)) as u32)
                    .copied(),
            );
            step_character(
                world,
                &mut actor.state,
                &actor.body,
                &actor.profile,
                &mut intent,
                FIXED_DT,
                &step_bodies,
            );
            self.step_scratch = step_bodies;
            actor.intent = intent;
        }
    }

    /// Resolve one melee swing per attacker per tick: players swing along their
    /// input yaw/pitch, actors along their state yaw with level aim. Cooldowns
    /// apply to misses and hits alike; dead players and noclip attackers cannot
    /// swing.
    pub(super) fn resolve_attacks(&mut self, world: &VoxelWorld) {
        let melee = self.packages.melee_table();
        let attackers: Vec<u64> = self
            .players
            .iter()
            .filter(|(_, player)| {
                !player.health.is_depleted()
                    && !player.state.motion.noclip
                    && player.input.attack
                    && self.tick >= player.attack_ready
                    // Registered weapons require ownership; hands, blocks and
                    // the bow swing the unarmed default as before.
                    && melee
                        .spec(player.input.selected)
                        .is_none_or(|_| player.inventory.count(player.input.selected) > 0)
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
            let spec = melee
                .spec(self.players[&id].input.selected)
                .unwrap_or(gameplay::combat::MELEE_HANDS);
            let player = &self.players[&id];
            let origin = player.state.motion.position + Vec3::Y * EYE_HEIGHT;
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
                        **other != id && !player.health.is_depleted() && !player.state.motion.noclip
                    })
                    .map(|(&other, player)| SwingTarget {
                        id: other,
                        position: player.state.motion.position,
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
                    let was_depleted = actor.health.is_depleted();
                    let amount = actor.health.damage(hit.damage);
                    if actor.health.is_depleted() {
                        actor.respawn_at = Some(self.tick + ACTOR_RESPAWN_TICKS);
                    }
                    push_event(
                        &mut self.events,
                        SimEvent::DamageDealt {
                            source: SimEntity::Player(id),
                            target: SimEntity::Actor(actor_id),
                            amount,
                            killed: actor.health.is_depleted(),
                        },
                    );
                    if !was_depleted && actor.health.is_depleted() {
                        push_event(
                            &mut self.events,
                            SimEvent::ActorDied {
                                id: actor_id,
                                killer: Some(SimEntity::Player(id)),
                            },
                        );
                    }
                }
            } else {
                // Impulse first: a killing blow zeroes momentum through damage_player.
                if let Some(victim) = self.players.get_mut(&hit.target) {
                    apply_player_impulse(&mut victim.state.motion, hit.impulse);
                }
                self.damage_player_event(SimEntity::Player(id), hit.target, hit.damage);
            }
        }
    }

    /// One actor swing: level aim along the actor's yaw against every other
    /// living actor and player. The innate species spec applies; `ITEM`
    /// actors may resolve equipment later. Consumes the intent edge and
    /// starts cooldown.
    fn swing_actor(&mut self, world: &VoxelWorld, id: u32) {
        let spec = self.actors[&id].kind.melee;
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
                .filter(|(_, player)| !player.health.is_depleted() && !player.state.motion.noclip)
                .map(|(&other, player)| SwingTarget {
                    id: other,
                    position: player.state.motion.position,
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
                let was_depleted = victim.health.is_depleted();
                let amount = victim.health.damage(hit.damage);
                if victim.health.is_depleted() {
                    victim.respawn_at = Some(self.tick + ACTOR_RESPAWN_TICKS);
                }
                push_event(
                    &mut self.events,
                    SimEvent::DamageDealt {
                        source: SimEntity::Actor(id),
                        target: SimEntity::Actor(target_id),
                        amount,
                        killed: victim.health.is_depleted(),
                    },
                );
                if !was_depleted && victim.health.is_depleted() {
                    push_event(
                        &mut self.events,
                        SimEvent::ActorDied {
                            id: target_id,
                            killer: Some(SimEntity::Actor(id)),
                        },
                    );
                }
            }
        } else {
            if let Some(victim) = self.players.get_mut(&hit.target) {
                apply_player_impulse(&mut victim.state.motion, hit.impulse);
            }
            self.damage_player_event(SimEntity::Actor(id), hit.target, hit.damage);
        }
    }
    /// All actor snapshots bucketed by their chunk coordinate, computed once
    /// per snapshot tick. Recipients gather from the buckets covering their
    /// interest set instead of rescanning every actor.
    pub(super) fn actor_snapshot_buckets(&self) -> HashMap<IVec3, Vec<ActorSnapshot>> {
        let mut buckets: HashMap<IVec3, Vec<ActorSnapshot>> = HashMap::new();
        for (&id, actor) in &self.actors {
            let chunk = voxel_world::chunk_coord(actor.state.motion.position.floor().as_ivec3());
            buckets.entry(chunk).or_default().push(actor.snapshot(id));
        }
        buckets
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

/// Deterministic order so brains and events see a stable actor sequence.
pub(super) type ActorMap = BTreeMap<u32, Actor>;

/// Bounded push: a full backlog drops the oldest event first. A free function
/// so hit resolution can push while holding an `actors` entry borrow.
fn push_event(events: &mut VecDeque<SimEvent>, event: SimEvent) {
    if events.len() == MAX_QUEUED_EVENTS {
        events.pop_front();
    }
    events.push_back(event);
}
