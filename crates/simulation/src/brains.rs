//! Scripted actor policies and learned-policy hooks. See `docs/controller.md`.
//!
//! Every brain consumes the same `ActorObservation` a learned policy sees, so
//! recorded demos are valid imitation data. All steering reduces to a yaw
//! error against the motor convention: yaw zero faces -Z, positive turn
//! rotates left.
use crate::actors::{ActorObservation, BrainKind, ObservedEntity};
use controller::CharacterIntent;
use glam::{Vec2, Vec3};

/// Distance at which a `FleeBrain` treats an entity as a threat.
const FLEE_RADIUS: f32 = 12.0;
/// Wander drift: half-throttle forward with a slow left sweep.
const WANDER_SPEED: f32 = 0.5;
const WANDER_TURN: f32 = 0.15;

/// One decision step for an actor: observation in, intent out. Scripted brains
/// consume the same observation a learned policy sees so recorded demos are
/// valid imitation data. The tick path owns masking and motor application.
pub trait Brain: Send + Sync {
    fn act(&mut self, obs: &ActorObservation, tick: u64) -> CharacterIntent;
}

/// Construct the runtime brain an `ActorKind` requests. `External` intents
/// arrive through the network path instead; map it to `Idle` defensively.
pub fn build_brain(kind: BrainKind) -> Box<dyn Brain> {
    match kind {
        BrainKind::Hunter => Box::new(HunterBrain),
        BrainKind::Flee => Box::new(FleeBrain),
        BrainKind::External | BrainKind::Idle => Box::new(IdleBrain),
    }
}

/// Wrap an angle into [-PI, PI).
fn wrapped(angle: f32) -> f32 {
    (angle + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI
}

/// World yaw that faces `to` from `from`, matching `look_direction` at yaw.
fn bearing(from: Vec3, to: Vec3) -> f32 {
    (-(to.x - from.x)).atan2(-(to.z - from.z))
}

/// Held turn steering `obs_yaw` toward `target`, clamped like the motor's bound.
fn face_turn(obs_yaw: f32, self_pos: Vec3, target: Vec3) -> f32 {
    wrapped(bearing(self_pos, target) - obs_yaw).clamp(-1.0, 1.0)
}

/// Held turn steering `obs_yaw` to face directly away from `threat`.
fn flee_turn(obs_yaw: f32, self_pos: Vec3, threat: Vec3) -> f32 {
    wrapped(bearing(threat, self_pos) - obs_yaw).clamp(-1.0, 1.0)
}

/// The closer of the two observed entities, or `None` when neither exists.
fn nearest(obs: &ActorObservation) -> Option<ObservedEntity> {
    match (obs.nearest_player, obs.nearest_actor) {
        (Some(player), Some(actor)) => {
            Some(if player.distance <= actor.distance {
                player
            } else {
                actor
            })
        }
        (player, actor) => player.or(actor),
    }
}

/// Slow exploratory drift shared by brains with nothing actionable.
fn wander() -> CharacterIntent {
    CharacterIntent {
        movement: Vec2::new(0.0, WANDER_SPEED),
        turn: WANDER_TURN,
        ..CharacterIntent::default()
    }
}

/// The training dummy: stands still, never decides anything.
struct IdleBrain;
impl Brain for IdleBrain {
    fn act(&mut self, _obs: &ActorObservation, _tick: u64) -> CharacterIntent {
        CharacterIntent::default()
    }
}

/// Melee pursuit: close on the nearest visible entity and swing on cooldown.
/// Also fires slot 0 (an area ability for the titan) when the target is in
/// reach and the slot is off cooldown. Without a line-of-sight target it
/// wanders, sweeping for new victims.
struct HunterBrain;
impl Brain for HunterBrain {
    fn act(&mut self, obs: &ActorObservation, _tick: u64) -> CharacterIntent {
        let Some(target) = nearest(obs) else {
            return wander();
        };
        if !target.line_of_sight {
            return wander();
        }
        let ability = [obs.cooldowns[0] == 0 && target.distance < 6.0, false, false];
        if target.distance > obs.melee_range * 0.9 {
            CharacterIntent {
                movement: Vec2::new(0.0, 1.0),
                turn: face_turn(obs.yaw, obs.position, target.position),
                ability,
                ..CharacterIntent::default()
            }
        } else {
            CharacterIntent {
                turn: face_turn(obs.yaw, obs.position, target.position),
                attack: obs.attack_ready,
                ability,
                ..CharacterIntent::default()
            }
        }
    }
}

/// Runs from any entity within `FLEE_RADIUS`, sensing through walls.
/// Otherwise shares the hunter's wander.
struct FleeBrain;
impl Brain for FleeBrain {
    fn act(&mut self, obs: &ActorObservation, _tick: u64) -> CharacterIntent {
        let Some(threat) = nearest(obs) else {
            return wander();
        };
        if threat.distance > FLEE_RADIUS {
            return wander();
        }
        CharacterIntent {
            movement: Vec2::new(0.0, 1.0),
            turn: flee_turn(obs.yaw, obs.position, threat.position),
            ..CharacterIntent::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::SimEntity;
    use controller::{MAX_SLOTS, Mode, StatusList};
    use gameplay::Health;

    fn observed(entity: SimEntity, position: Vec3, distance: f32, los: bool) -> ObservedEntity {
        ObservedEntity {
            entity,
            position,
            velocity: Vec3::ZERO,
            distance,
            line_of_sight: los,
        }
    }

    /// Actor at the origin facing -Z (yaw 0) with a two-block melee reach.
    fn observation() -> ActorObservation {
        ActorObservation {
            tick: 0,
            position: Vec3::ZERO,
            velocity: Vec3::ZERO,
            yaw: 0.0,
            grounded: true,
            health: Health::default(),
            attack_ready: false,
            melee_range: 2.0,
            nearest_player: None,
            nearest_actor: None,
            mode: Mode::default(),
            statuses: StatusList::default(),
            cooldowns: [0; MAX_SLOTS],
            cast: None,
            crouching: false,
        }
    }

    #[test]
    fn idle_brain_emits_default_intent() {
        let mut brain = build_brain(BrainKind::Idle);
        assert_eq!(brain.act(&observation(), 3), CharacterIntent::default());
        // External actors are never built; defensive mapping stays inert.
        let mut brain = build_brain(BrainKind::External);
        assert_eq!(brain.act(&observation(), 3), CharacterIntent::default());
    }

    #[test]
    fn hunter_wanders_without_target_or_los() {
        let mut brain = build_brain(BrainKind::Hunter);
        let no_target = brain.act(&observation(), 0);
        assert_eq!(no_target.movement, Vec2::new(0.0, WANDER_SPEED));
        assert_eq!(no_target.turn, WANDER_TURN);
        assert!(!no_target.attack);
        // A target exists but terrain blocks the eye ray.
        let mut obs = observation();
        obs.nearest_player = Some(observed(SimEntity::Player(7), Vec3::new(0.0, 0.0, -4.0), 4.0, false));
        let blocked = brain.act(&obs, 0);
        assert_eq!(blocked, no_target);
    }

    #[test]
    fn hunter_pursues_visible_target() {
        let mut brain = build_brain(BrainKind::Hunter);
        let mut obs = observation();
        // Straight ahead at -Z, out of reach: full forward, no turn.
        obs.nearest_player = Some(observed(SimEntity::Player(7), Vec3::new(0.0, 0.0, -5.0), 5.0, true));
        let intent = brain.act(&obs, 0);
        assert_eq!(intent.movement, Vec2::new(0.0, 1.0));
        assert!(intent.turn.abs() < 1e-6);
        assert!(!intent.attack);
        // Target to the +X right: steering toward it is a negative turn.
        obs.nearest_player = Some(observed(SimEntity::Player(7), Vec3::new(5.0, 0.0, 0.0), 5.0, true));
        let intent = brain.act(&obs, 0);
        assert_eq!(intent.movement, Vec2::new(0.0, 1.0));
        assert!(intent.turn < 0.0 && intent.turn >= -1.0);
    }

    #[test]
    fn hunter_prefers_closer_target_regardless_of_kind() {
        let mut brain = build_brain(BrainKind::Hunter);
        let mut obs = observation();
        // The actor is farther away but nearer than nothing; the player is
        // closest and straight ahead, so it wins.
        obs.nearest_actor = Some(observed(SimEntity::Actor(2), Vec3::new(9.0, 0.0, 0.0), 9.0, true));
        obs.nearest_player = Some(observed(SimEntity::Player(7), Vec3::new(0.0, 0.0, -5.0), 5.0, true));
        let intent = brain.act(&obs, 0);
        assert!(intent.turn.abs() < 1e-6, "faced the closer player");
        // Reversed distances: the actor is nearest, to the right.
        obs.nearest_player = Some(observed(SimEntity::Player(7), Vec3::new(0.0, 0.0, -9.0), 9.0, true));
        obs.nearest_actor = Some(observed(SimEntity::Actor(2), Vec3::new(5.0, 0.0, 0.0), 5.0, true));
        let intent = brain.act(&obs, 0);
        assert!(intent.turn < 0.0, "turned toward the closer actor");
    }

    #[test]
    fn hunter_attacks_only_in_range_when_ready() {
        let mut brain = build_brain(BrainKind::Hunter);
        let mut obs = observation();
        obs.nearest_player = Some(observed(SimEntity::Player(7), Vec3::new(0.0, 0.0, -1.0), 1.0, true));
        // In range but not ready: stands ground, holds the swing.
        let intent = brain.act(&obs, 0);
        assert_eq!(intent.movement, Vec2::ZERO);
        assert!(!intent.attack);
        // Ready edge fires exactly once per observation.
        obs.attack_ready = true;
        let intent = brain.act(&obs, 0);
        assert!(intent.attack);
        assert_eq!(intent.movement, Vec2::ZERO);
        // At the range boundary 0.9*range = 1.8, just outside still pursues.
        obs.nearest_player = Some(observed(SimEntity::Player(7), Vec3::new(0.0, 0.0, -1.9), 1.9, true));
        let intent = brain.act(&obs, 0);
        assert_eq!(intent.movement, Vec2::new(0.0, 1.0));
        assert!(!intent.attack);
    }

    #[test]
    fn flee_runs_away_from_near_threat_ignoring_los() {
        let mut brain = build_brain(BrainKind::Flee);
        let mut obs = observation();
        // Threat directly ahead within radius, LOS blocked: still flees,
        // turning hard around since away is behind the actor.
        obs.nearest_player = Some(observed(SimEntity::Player(7), Vec3::new(0.0, 0.0, -4.0), 4.0, false));
        let intent = brain.act(&obs, 0);
        assert_eq!(intent.movement, Vec2::new(0.0, 1.0));
        assert!(intent.turn.abs() == 1.0, "away is a full turn from facing the threat");
        // Threat directly behind: flee bearing is straight ahead.
        obs.nearest_player = Some(observed(SimEntity::Player(7), Vec3::new(0.0, 0.0, 4.0), 4.0, true));
        let intent = brain.act(&obs, 0);
        assert!(intent.turn.abs() < 1e-6);
        // Threat to the right at +X: away bearing is -X, a left turn.
        obs.nearest_player = Some(observed(SimEntity::Player(7), Vec3::new(4.0, 0.0, 0.0), 4.0, true));
        let intent = brain.act(&obs, 0);
        assert!(intent.turn > 0.0 && intent.turn <= 1.0);
    }

    #[test]
    fn flee_wanders_when_no_threat_within_radius() {
        let mut brain = build_brain(BrainKind::Flee);
        let mut obs = observation();
        obs.nearest_player = Some(observed(SimEntity::Player(7), Vec3::new(0.0, 0.0, -30.0), 30.0, true));
        let intent = brain.act(&obs, 0);
        assert_eq!(intent.movement, Vec2::new(0.0, WANDER_SPEED));
        assert_eq!(intent.turn, WANDER_TURN);
        assert!(!intent.attack);
        // Empty observation wanders identically.
        assert_eq!(brain.act(&observation(), 0), intent);
    }

    #[test]
    fn wrapped_angle_normalizes_to_signed_pi() {
        use std::f32::consts::{FRAC_PI_2, PI, TAU};
        assert!((wrapped(0.0) - 0.0).abs() < 1e-7);
        assert!((wrapped(3.0 * PI) + PI).abs() < 1e-6);
        assert!((wrapped(FRAC_PI_2 + TAU) - FRAC_PI_2).abs() < 1e-6);
    }

    #[test]
    fn bearing_matches_look_direction_at_yaw() {
        // look_direction(yaw, 0) == (-sin yaw, 0, -cos yaw).
        for &yaw in &[0.0, 0.7, -1.3, std::f32::consts::PI] {
            let direction = physics::look_direction(yaw, 0.0);
            let target = Vec3::ZERO + direction * 3.0;
            assert!((wrapped(bearing(Vec3::ZERO, target) - yaw)).abs() < 1e-5);
        }
    }
}
